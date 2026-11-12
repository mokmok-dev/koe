//! The main window, built from gpui-kit components.

use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext as _, Axis, Context, ElementId, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, TitleBar,
    alert::Alert,
    bubble::{Bubble, BubbleVariant},
    button::{Button, ButtonVariants as _},
    group_box::{GroupBox, GroupBoxVariant, GroupBoxVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    message::{Message, MessageAlignment, MessageContent, MessageHeader},
    progress::Progress,
    scroll::ScrollableElement as _,
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    sidebar::{Sidebar, SidebarHeader, SidebarMenuItem, SidebarToggleButton},
    status_bar::StatusBar,
    v_flex,
};
use koe_capture::Source;
use koe_record::Format;

use crate::app::{Screen, Shell};
use crate::dictation::Dictation;
use crate::hotkey;
use crate::hud;
use crate::language;
use crate::settings;
use crate::state::{Done, Phase, Sources};
use crate::transcript::Speaker;

const LIVE: u32 = 0x007E_E787;
const IDLE: u32 = 0x005A_5A62;
const RED: u32 = 0x00FF_5A5F;
const AMBER: u32 = 0x00F0_B429;
const BLUE: u32 = 0x0058_A6FF;

/// The language picker's rows: the display names, mapped back to the tags the
/// settings file stores by [`language::tag_of`].
type LanguageItems = SearchableVec<SharedString>;

/// Builds a click handler that applies a shell mutation. It is `Clone` so that
/// setting renderers, which run as `Fn`, can copy it before a control takes one.
fn click(
    shell: Entity<Shell>,
    apply: impl Fn(&mut Shell, &mut Context<Shell>) + Clone + 'static,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + Clone + 'static {
    move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
        shell.update(cx, |shell, cx| apply(shell, cx));
    }
}

/// The main window's root view.
pub struct MainView {
    shell: Entity<Shell>,
    hotkey_input: Entity<InputState>,
    model_input: Entity<InputState>,
    language_select: Entity<SelectState<LanguageItems>>,
    /// The meeting thread's scroll position.
    thread_scroll: ScrollHandle,
    /// Utterance count, draft count, and draft text length last scrolled for.
    thread_signature: (usize, usize, usize),
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    /// Builds the view, seeds the settings fields, and re-renders on change.
    pub fn new(shell: Entity<Shell>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (hotkey_input, model_input, language_select) =
            Self::settings_fields(&shell, window, cx);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&hotkey_input, {
            let shell = shell.clone();
            move |_, input, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let draft = input.read(cx).value().to_string();
                    shell.update(cx, |shell, cx| shell.apply_hotkey(&draft, cx));
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        }));
        subscriptions.push(cx.subscribe(&model_input, {
            let shell = shell.clone();
            let model_input = model_input.clone();
            move |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let model = settings::value_or_none(&model_input.read(cx).value())
                        .map(std::path::PathBuf::from);
                    let language = shell.read(cx).dictation_control.settings.language.clone();
                    shell.update(cx, |shell, cx| {
                        shell.apply_dictation_settings(model, language, cx);
                    });
                }
            }
        }));
        subscriptions.push(cx.subscribe(&language_select, {
            let shell = shell.clone();
            move |_, _, event: &SelectEvent<LanguageItems>, cx| {
                let SelectEvent::Confirm(Some(name)) = event else {
                    return;
                };
                let Some(tag) = language::tag_of(name) else {
                    return;
                };
                let model = shell.read(cx).dictation_control.settings.model.clone();
                let language = settings::value_or_none(tag).map(str::to_owned);
                shell.update(cx, |shell, cx| {
                    shell.apply_dictation_settings(model, language, cx);
                });
            }
        }));

        cx.observe(&shell, |_, _, cx| cx.notify()).detach();

        Self {
            shell,
            hotkey_input,
            model_input,
            language_select,
            thread_scroll: ScrollHandle::default(),
            thread_signature: (0, 0, 0),
            _subscriptions: subscriptions,
        }
    }

    /// Seeds the dictation fields from the saved settings.
    fn settings_fields(
        shell: &Entity<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<InputState>,
        Entity<InputState>,
        Entity<SelectState<LanguageItems>>,
    ) {
        let (hotkey_value, model_value, language_row) = {
            let shell = shell.read(cx);
            let settings = &shell.dictation_control.settings;
            let saved = settings.language.as_deref().unwrap_or_default();
            (
                hotkey::display(&settings.hotkey),
                settings
                    .model
                    .as_ref()
                    .map_or_else(String::new, |model| model.display().to_string()),
                language::CHOICES.iter().position(|(tag, _)| *tag == saved),
            )
        };

        let hotkey_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Ctrl+Shift+Space")
                .default_value(hotkey_value)
        });
        let model_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/to/model.gguf")
                .default_value(model_value)
        });
        let language_items: Vec<SharedString> = language::CHOICES
            .iter()
            .map(|(_, name)| SharedString::from(*name))
            .collect();
        let language_select = cx.new(|cx| {
            SelectState::new(LanguageItems::new(language_items), None, window, cx).searchable(true)
        });
        // Unlike the constructor, `set_selected_index` records the scroll that
        // brings the stored language into view on the first open.
        language_select.update(cx, |state, cx| {
            state.set_selected_index(language_row.map(IndexPath::new), window, cx);
        });

        (hotkey_input, model_input, language_select)
    }
}

impl MainView {
    /// The take controls shared by the meeting and recorder screens: the phase
    /// card, the meters, and the start or stop button.
    fn take_panel(&self, shell: &Shell, cx: &Context<Self>) -> AnyElement {
        let entity = self.shell.clone();
        let theme = cx.theme();
        let phase = &shell.phase;
        let active = phase.is_active();
        let can_start = phase.can_start();
        let can_stop = phase.can_stop();
        let elapsed = phase.elapsed(Instant::now());
        let live = phase.live();
        let levels = shell.levels;
        let detail = detail(phase);
        let tone = phase_tone(phase);

        let (primary_label, primary_enabled, primary_stop) = if can_stop {
            ("Stop recording", true, true)
        } else if can_start {
            ("Start recording", !shell.settings.sources.is_empty(), false)
        } else {
            ("Saving...", false, false)
        };

        let on_primary = click(entity, move |shell, cx| {
            if primary_stop {
                shell.stop_recording(cx);
            } else {
                shell.start_recording(cx);
            }
        });

        let primary = Button::new("primary")
            .label(primary_label)
            .disabled(!primary_enabled)
            .w_full()
            .h(px(44.0))
            .on_click(on_primary);
        let primary = if primary_stop {
            primary.danger()
        } else {
            primary.primary()
        };

        v_flex()
            .gap(px(14.0))
            .child(
                GroupBox::new().outline().child(
                    v_flex()
                        .gap(px(10.0))
                        .child(
                            h_flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(div().size(px(14.0)).rounded_full().bg(tone))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .child(
                                            div()
                                                .text_base()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(phase.label()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(detail),
                                        ),
                                )
                                .children(elapsed.map(|elapsed| {
                                    div()
                                        .text_2xl()
                                        .font_family("monospace")
                                        .child(hud::elapsed_text(elapsed))
                                })),
                        )
                        .children(active.then(|| {
                            v_flex()
                                .gap_2()
                                .child(meter("Mic", Source::Microphone, levels, live))
                                .child(meter("System", Source::SystemAudio, levels, live))
                        })),
                ),
            )
            .child(primary)
            .into_any_element()
    }

    /// The take controls over the meeting transcript, read as a message thread.
    fn meeting_screen(&mut self, shell: &Shell, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .gap(px(14.0))
            .child(self.take_panel(shell, cx))
            .child(self.transcript_panel(shell, cx))
            .into_any_element()
    }

    /// The retained transcript, or the empty state that says what will fill it.
    fn transcript_panel(&mut self, shell: &Shell, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let count = shell.transcript.len();
        let count_label = format!("{count} utterance{}", if count == 1 { "" } else { "s" });
        let title = if shell.transcript_pending {
            "Previous take"
        } else {
            "Transcript"
        };
        self.follow_latest(shell);

        let thread = if shell.transcript.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child("The meeting appears here as it is transcribed."),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(
                    "Start recording to hear You on the microphone and Them on the system audio.",
                ))
                .into_any_element()
        } else {
            self.message_thread(shell)
        };

        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .gap(px(8.0))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .children((count > 0).then(|| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(count_label)
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .child(thread),
            )
            .into_any_element()
    }

    /// Keeps the newest row in view as the take writes more.
    fn follow_latest(&mut self, shell: &Shell) {
        let drafts = shell.transcript.drafts().count();
        let signature = (
            shell.transcript.len(),
            drafts,
            shell
                .transcript
                .drafts()
                .map(|draft| draft.text.len())
                .sum::<usize>(),
        );
        if signature == self.thread_signature {
            return;
        }
        self.thread_signature = signature;
        let rows = shell.transcript.len() + drafts;
        if rows > 0 {
            self.thread_scroll.scroll_to_item(rows - 1);
        }
    }

    /// The retained utterances and the drafts under them, newest at the bottom.
    fn message_thread(&self, shell: &Shell) -> AnyElement {
        let drafts = shell.transcript.drafts();
        v_flex()
            .id("meeting-thread")
            .size_full()
            .min_h(px(0.0))
            .gap(px(12.0))
            .p(px(14.0))
            .overflow_y_scroll()
            .track_scroll(&self.thread_scroll)
            .children(shell.transcript.utterances().iter().map(|utterance| {
                message_row(
                    ("utterance", utterance.sequence),
                    utterance.speaker,
                    &utterance.clock,
                    &utterance.text,
                    bubble_variant(utterance.speaker),
                    false,
                )
            }))
            .children(drafts.map(|draft| {
                message_row(
                    ("draft", draft.speaker.index() as u64),
                    draft.speaker,
                    &draft.clock,
                    &draft.text,
                    BubbleVariant::Muted,
                    true,
                )
            }))
            // Last, so the row indices the scroll handle targets stay 0..rows.
            .vertical_scrollbar(&self.thread_scroll)
            .into_any_element()
    }

    /// The settings pages.
    ///
    /// Rows that own an `InputState` render through `SettingField::render`
    /// rather than `SettingField::input`: an input field commits on every
    /// keystroke, and the hotkey must commit only on Enter.
    #[allow(
        clippy::too_many_lines,
        reason = "the two pages are one declarative tree; splitting off a row would separate it from the page that owns it"
    )]
    fn settings_screen(&self, shell: &Shell, cx: &Context<Self>) -> AnyElement {
        let entity = self.shell.clone();
        let can_start = shell.phase.can_start();
        let format = shell.settings.format;
        let directory = shell.settings.directory.display().to_string();
        let dictation_done = matches!(shell.dictation_control.dictation, Dictation::Done(_));
        let status = dictation_status(shell);
        let hotkey_text = self.hotkey_input.read(cx).value().to_string();
        let model_text = self.model_input.read(cx).value().to_string();
        let hotkey_hint = hotkey_hint(&hotkey_text, shell);
        let model_hint = model_hint(&model_text, shell);

        let on_ogg = click(entity.clone(), |shell, cx| {
            shell.settings.format = Format::Ogg;
            cx.notify();
        });
        let on_wav = click(entity.clone(), |shell, cx| {
            shell.settings.format = Format::Wav;
            cx.notify();
        });
        let on_open = click(entity.clone(), Shell::open_folder);
        let on_clear = click(entity.clone(), Shell::dismiss_dictation);

        let status_item = SettingItem::render(move |_, _, _| {
            v_flex()
                .gap_2()
                .child(div().text_sm().child(status.clone()))
                .children(dictation_done.then(|| {
                    Button::new("clear-dictation")
                        .label("Clear")
                        .small()
                        .on_click(on_clear.clone())
                }))
        })
        .keywords(["dictation", "status", "clear"]);

        let hotkey_item = SettingItem::new(
            "Hotkey",
            SettingField::render({
                let input = self.hotkey_input.clone();
                move |_, _, cx| {
                    v_flex()
                        .gap_1()
                        .child(Input::new(&input).aria_label("Hotkey").w(px(240.0)))
                        .children(hotkey_hint.clone().map(|(message, error)| {
                            let hint = div().text_xs().child(message);
                            if error {
                                hint.text_color(rgb(RED)).into_any_element()
                            } else {
                                hint.text_color(cx.theme().muted_foreground)
                                    .into_any_element()
                            }
                        }))
                }
            }),
        );

        let model_item = SettingItem::new(
            "Model",
            SettingField::render({
                let input = self.model_input.clone();
                move |_, _, cx| {
                    v_flex()
                        .gap_1()
                        .child(Input::new(&input).aria_label("Model").w(px(240.0)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(model_hint.clone()),
                        )
                }
            }),
        );

        let language_item = SettingItem::new(
            "Language",
            SettingField::render({
                let select = self.language_select.clone();
                let saved = shell
                    .dictation_control
                    .settings
                    .language
                    .clone()
                    .unwrap_or_default();
                let label = language::CHOICES
                    .iter()
                    .find(|(tag, _)| *tag == saved.as_str())
                    .map_or_else(|| saved.clone(), |(_, name)| (*name).to_owned());
                move |options, _, _| {
                    Select::new(&select)
                        .placeholder(label.clone())
                        .search_placeholder("Search languages…")
                        .accessibility_label("Language")
                        .disabled(options.is_disabled())
                }
            }),
        )
        .description("The speech model must support the chosen language.")
        .layout(Axis::Vertical)
        .keywords(["language", "locale", "speech"]);

        let source_item = |source: Source, title: &'static str| {
            SettingItem::new(
                title,
                SettingField::checkbox(
                    {
                        let shell = entity.clone();
                        move |cx: &App| shell.read(cx).settings.sources.contains(source)
                    },
                    {
                        let shell = entity.clone();
                        move |checked: bool, cx: &mut App| {
                            shell.update(cx, |shell, cx| {
                                shell.settings.sources = if checked {
                                    shell.settings.sources.with(source)
                                } else {
                                    shell.settings.sources.without(source)
                                };
                                cx.notify();
                            });
                        }
                    },
                ),
            )
            .disabled(!can_start)
        };

        let format_item = SettingItem::new(
            "File",
            SettingField::render(move |options, _, _| {
                h_flex()
                    .gap_2()
                    .child(
                        if format == Format::Ogg {
                            Button::new("format-ogg").primary()
                        } else {
                            Button::new("format-ogg").ghost()
                        }
                        .label("Ogg Opus")
                        .small()
                        .disabled(options.is_disabled())
                        .on_click(on_ogg.clone()),
                    )
                    .child(
                        if format == Format::Wav {
                            Button::new("format-wav").primary()
                        } else {
                            Button::new("format-wav").ghost()
                        }
                        .label("WAV")
                        .small()
                        .disabled(options.is_disabled())
                        .on_click(on_wav.clone()),
                    )
            }),
        )
        .keywords(["format", "ogg", "wav", "opus"])
        .disabled(!can_start);

        let folder_item = SettingItem::new(
            "Location",
            SettingField::render(move |_, _, cx| {
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .max_w(px(200.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_sm()
                            .font_family("monospace")
                            .text_color(cx.theme().muted_foreground)
                            .child(directory.clone()),
                    )
                    .child(
                        Button::new("open-folder")
                            .label("Open")
                            .small()
                            .on_click(on_open.clone()),
                    )
            }),
        )
        .keywords(["folder", "directory", "path", "recordings"]);

        Settings::new("koe-settings")
            .with_group_variant(GroupBoxVariant::Outline)
            .sidebar_width(px(180.0))
            .sidebar_size_range(px(150.0)..px(260.0))
            .page(
                SettingPage::new("Dictation")
                    .icon(Icon::new(IconName::Mic))
                    .description("The global binding and the speech model dictation and meeting transcripts use.")
                    .resettable(false)
                    .group(SettingGroup::new().title("Status").item(status_item))
                    .group(SettingGroup::new().title("Binding").item(hotkey_item))
                    .group(
                        SettingGroup::new()
                            .title("Speech model")
                            .item(model_item)
                            .item(language_item),
                    ),
            )
            .page(
                SettingPage::new("Recording")
                    .icon(Icon::new(IconName::FolderOpen))
                    .description("What a take captures, how it is written, and where it goes.")
                    .resettable(false)
                    .group(
                        SettingGroup::new()
                            .title("Sources")
                            .item(source_item(Source::Microphone, "Microphone"))
                            .item(source_item(Source::SystemAudio, "System audio")),
                    )
                    .group(SettingGroup::new().title("Format").item(format_item))
                    .group(SettingGroup::new().title("Folder").item(folder_item)),
            )
            .into_any_element()
    }
}

impl Render for MainView {
    #[allow(
        clippy::too_many_lines,
        reason = "the window chrome is one element tree; splitting the title bar, body and footer scatters the layout"
    )]
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = self.shell.clone();
        let shell = entity.read(cx);
        let theme = cx.theme();

        let phase = &shell.phase;
        let tone = phase_tone(phase);
        let badge = if shell.dictation_control.dictation.is_active() {
            "DICTATING".to_owned()
        } else {
            phase.label().to_uppercase()
        };
        let screen = shell.screen;
        let sidebar_collapsed = shell.sidebar_collapsed;
        let notice = shell.notice.clone();
        let tray_available = shell.tray.is_some();
        let tray_error = shell.tray_error.clone();
        let hotkey_error = shell
            .dictation_control
            .hotkeys
            .is_none()
            .then(|| shell.dictation_control.hotkey_error.clone())
            .flatten();
        let done = match phase {
            Phase::Done(done) => Some(done_view(done)),
            _ => None,
        };

        let on_toggle_sidebar = click(entity.clone(), Shell::toggle_sidebar);
        let on_meeting = click(entity.clone(), Shell::show_meeting);
        let on_recorder = click(entity.clone(), Shell::show_recorder);
        let on_settings = click(entity.clone(), Shell::show_settings);
        let on_dismiss = click(entity.clone(), Shell::dismiss);

        let mut alerts: Vec<AnyElement> = Vec::new();
        if let Some(notice) = notice {
            alerts.push(Alert::warning("notice", notice).banner().into_any_element());
        }
        if let Some(error) = hotkey_error {
            alerts.push(
                v_flex()
                    .gap_1()
                    .child(
                        Alert::warning(
                            "hotkey-missing",
                            "No global hotkey. Dictation needs a desktop that lets apps register shortcuts; on Linux that means X11. Press Enter in the Hotkey field to retry.",
                        )
                        .banner(),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(error),
                    )
                    .into_any_element(),
            );
        }
        if !tray_available {
            alerts.push(
                v_flex()
                    .gap_1()
                    .child(
                        Alert::warning(
                            "tray-missing",
                            "No menu bar icon. Keep this window open to control recordings.",
                        )
                        .banner(),
                    )
                    .children(tray_error.map(|error| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(error)
                    }))
                    .into_any_element(),
            );
        }

        let done_element = done.map(|done| done_card(done, on_dismiss, cx));

        let content = match screen {
            Screen::Meeting => v_flex()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .p(px(22.0))
                .gap(px(14.0))
                .child(self.meeting_screen(shell, cx))
                .into_any_element(),
            Screen::Recorder => v_flex()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .overflow_y_scrollbar()
                .p(px(22.0))
                .gap(px(14.0))
                .child(self.take_panel(shell, cx))
                .children(alerts)
                .children(done_element)
                .into_any_element(),
            Screen::Settings => v_flex()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .p(px(22.0))
                .gap(px(14.0))
                .children(alerts)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .child(self.settings_screen(shell, cx)),
                )
                .children(done_element)
                .into_any_element(),
        };

        let sidebar = Sidebar::new("screens")
            .collapsed(sidebar_collapsed)
            .w(px(200.0))
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(IconName::Mic))
                        .children((!sidebar_collapsed).then_some("koe")),
                ),
            )
            .child(
                SidebarMenuItem::new("Meeting")
                    .icon(Icon::new(gpui_kit::assets::IconName::MessageCircle))
                    .active(screen == Screen::Meeting)
                    .on_click(on_meeting),
            )
            .child(
                SidebarMenuItem::new("Recorder")
                    .icon(Icon::new(IconName::Mic))
                    .active(screen == Screen::Recorder)
                    .on_click(on_recorder),
            )
            .child(
                SidebarMenuItem::new("Settings")
                    .icon(Icon::new(IconName::Settings))
                    .active(screen == Screen::Settings)
                    .on_click(on_settings),
            );

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child("koe"))
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.0))
                    // `h_flex` centres its children; without the stretch the
                    // settings page's scroll region collapses to its header
                    // and the groups never paint.
                    .items_stretch()
                    .child(sidebar)
                    .child(content),
            )
            .child(
                StatusBar::new()
                    .left(
                        SidebarToggleButton::new()
                            .collapsed(sidebar_collapsed)
                            .accessibility_label(if sidebar_collapsed {
                                "Expand the sidebar"
                            } else {
                                "Collapse the sidebar"
                            })
                            .on_click(on_toggle_sidebar),
                    )
                    .right(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tone)
                            .child(badge),
                    ),
            )
    }
}

/// The status sentence the dictation section shows.
fn dictation_status(shell: &Shell) -> String {
    let binding = hotkey::display(&shell.dictation_control.settings.hotkey);
    match &shell.dictation_control.dictation {
        Dictation::Idle => format!("Press {binding} anywhere to dictate"),
        Dictation::Listening { .. } => {
            let elapsed = shell
                .dictation_control
                .dictation
                .elapsed(Instant::now())
                .map_or_else(String::new, hud::elapsed_text);
            format!("Dictating {elapsed} — press {binding} to stop")
        }
        Dictation::Stopping { .. } => "Transcribing — the text follows".to_owned(),
        Dictation::Done(done) if done.failed() => done.failure.as_ref().map_or_else(
            || "Dictation heard nothing".to_owned(),
            |failure| format!("{}: {failure}", failure.headline()),
        ),
        Dictation::Done(done) => format!("Typed {}", done.text),
    }
}

/// The parser's complaint, or the cue that Enter commits the binding.
fn hotkey_hint(text: &str, shell: &Shell) -> Option<(String, bool)> {
    let draft = settings::value_or_none(text)?;
    match hotkey::parse(draft) {
        Err(error) => Some((error.to_string(), true)),
        Ok(hotkey) if hotkey != shell.dictation_control.settings.hotkey => {
            Some(("Press Enter to apply this binding".to_owned(), false))
        }
        Ok(_) => None,
    }
}

/// What the model field should explain on this platform.
fn model_hint(text: &str, shell: &Shell) -> String {
    #[cfg(target_os = "macos")]
    {
        let _ = shell;
        if settings::value_or_none(text).is_none() {
            return "Leave the model empty to use the system speech model for dictation and meetings"
                .to_owned();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        if settings::dictation_model(&shell.dictation_control.settings).is_none() {
            return "No transcription model set: dictation refuses to start and meeting takes record without a transcript"
                .to_owned();
        }
    }
    "The GGUF file dictation and meeting transcripts use".to_owned()
}

fn meter(name: &str, source: Source, levels: [f32; 2], live: Sources) -> impl IntoElement {
    let index = Sources::index(source);
    let level = levels[index];
    let level = if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(56.0))
                .text_xs()
                .text_color(rgb(IDLE))
                .child(name.to_owned()),
        )
        .child(
            Progress::new(format!("meter-{index}"))
                .value(level * 100.0)
                .color(rgb(if live.contains(source) { LIVE } else { IDLE }))
                .with_size(px(6.0))
                .flex_1(),
        )
}

/// One transcript row: a finalized utterance or a draft the engine is revising.
fn message_row(
    id: impl Into<ElementId>,
    speaker: Speaker,
    clock: &SharedString,
    text: &SharedString,
    variant: BubbleVariant,
    interim: bool,
) -> AnyElement {
    Message::new()
        .id(id)
        .role(Role::ListItem)
        .alignment(utterance_alignment(speaker))
        .header(
            MessageHeader::new()
                .child(speaker.label())
                .child("·")
                .child(clock.clone())
                .children(interim.then(|| div().text_xs().italic().child("interim"))),
        )
        .content(
            MessageContent::new().bubble(
                Bubble::new().with_variant(variant).child(
                    div()
                        .when(interim, gpui::Styled::italic)
                        .child(text.clone()),
                ),
            ),
        )
        .when(interim, |row| row.opacity(0.65))
        .into_any_element()
}

/// Your own words sit at the trailing edge, the way a sent message does; the
/// remote voices stay at the leading edge.
const fn utterance_alignment(speaker: Speaker) -> MessageAlignment {
    match speaker {
        Speaker::You => MessageAlignment::End,
        Speaker::Them => MessageAlignment::Start,
    }
}

/// Your own bubble carries the accent surface, so the two sides of the
/// conversation stay distinguishable without relying on position alone.
const fn bubble_variant(speaker: Speaker) -> BubbleVariant {
    match speaker {
        Speaker::You => BubbleVariant::Filled,
        Speaker::Them => BubbleVariant::Secondary,
    }
}

struct DoneView {
    output: String,
    failed: bool,
    empty: bool,
    session_removed: bool,
    summary: Option<String>,
    headline: Option<String>,
    failure: Option<String>,
    warning: Option<String>,
}

fn done_view(done: &Done) -> DoneView {
    DoneView {
        output: done.output.display().to_string(),
        failed: done.failed(),
        empty: done.summary.is_some_and(|summary| summary.frames == 0),
        session_removed: done.session_removed,
        summary: done.summary.map(|summary| {
            format!(
                "{} · {} frames",
                hud::elapsed_text(summary.duration),
                summary.frames
            )
        }),
        headline: done
            .failure
            .as_ref()
            .map(|failure| failure.headline().to_owned()),
        failure: done.failure.as_ref().map(ToString::to_string),
        warning: done.warning.clone(),
    }
}

fn done_card(
    done: DoneView,
    on_dismiss: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<MainView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let colour = if done.failed { rgb(RED) } else { rgb(LIVE) };
    GroupBox::new().outline().child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colour)
                    .child(if done.failed {
                        "Take failed"
                    } else {
                        "Last take"
                    }),
            )
            .children(done.empty.then(|| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if done.session_removed {
                        "No audio was written, so the take's session was removed."
                    } else {
                        "No audio was written, and the take's session could not be removed."
                    })
            }))
            .children((!done.empty).then(|| {
                div()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(theme.muted_foreground)
                    .child(done.output)
            }))
            .children(done.summary.map(|summary| div().text_xs().child(summary)))
            .children(
                done.headline
                    .map(|headline| div().text_xs().text_color(rgb(RED)).child(headline)),
            )
            .children(done.failure.map(|failure| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(failure)
            }))
            .children(
                done.warning
                    .map(|warning| div().text_xs().text_color(rgb(AMBER)).child(warning)),
            )
            .child(
                Button::new("dismiss")
                    .label("Dismiss")
                    .small()
                    .on_click(on_dismiss),
            ),
    )
}

fn phase_tone(phase: &Phase) -> gpui::Rgba {
    match phase {
        Phase::Idle => rgb(IDLE),
        Phase::Arming { .. } | Phase::Paused(_) => rgb(AMBER),
        Phase::Recording(_) => rgb(RED),
        Phase::Stopping { .. } => rgb(BLUE),
        Phase::Done(done) if done.failed() => rgb(RED),
        Phase::Done(_) => rgb(LIVE),
    }
}

fn detail(phase: &Phase) -> String {
    match phase {
        Phase::Idle => "ready when you are".to_owned(),
        Phase::Arming { .. } => "opening sources".to_owned(),
        Phase::Recording(live) => format!("{} source(s) live", live.live.len()),
        Phase::Paused(live) => format!("paused, {} source(s) held", live.live.len()),
        Phase::Stopping { .. } => "finishing the file".to_owned(),
        Phase::Done(done) if done.failed() => done.failure.as_ref().map_or_else(
            || "the take did not complete".to_owned(),
            |failure| failure.headline().to_owned(),
        ),
        Phase::Done(_) => "saved to your recordings folder".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn you_sits_at_the_trailing_edge_and_them_at_the_leading_edge() {
        assert_eq!(utterance_alignment(Speaker::You), MessageAlignment::End);
        assert_eq!(utterance_alignment(Speaker::Them), MessageAlignment::Start);
        assert_eq!(bubble_variant(Speaker::You), BubbleVariant::Filled);
        assert_eq!(bubble_variant(Speaker::Them), BubbleVariant::Secondary);
    }
}
