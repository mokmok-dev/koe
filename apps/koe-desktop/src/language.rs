//! The languages the dictation picker offers.

/// The picker's entries: a whisper language code and the name it shows.
///
/// The empty code is Automatic, where the engine picks the language. The rest
/// follow in name order, and their codes are whisper's language table, which
/// is what the GGML backend matches a tag against. That table spells Javanese
/// `jw` rather than ISO 639-1's `jv`, so a hand-edited `jv` is rejected like
/// any other unknown tag. Neither backend accepts a tag it does not know, and
/// this list is all the picker can set: a regional variant such as `pt-BR` is
/// shown verbatim and left unselected, so changing it means editing the
/// settings file.
pub const CHOICES: &[(&str, &str)] = &[
    ("", "Automatic"),
    ("af", "Afrikaans"),
    ("sq", "Albanian"),
    ("am", "Amharic"),
    ("ar", "Arabic"),
    ("hy", "Armenian"),
    ("as", "Assamese"),
    ("az", "Azerbaijani"),
    ("ba", "Bashkir"),
    ("eu", "Basque"),
    ("be", "Belarusian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("br", "Breton"),
    ("bg", "Bulgarian"),
    ("my", "Burmese"),
    ("yue", "Cantonese"),
    ("ca", "Catalan"),
    ("zh", "Chinese"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("en", "English"),
    ("et", "Estonian"),
    ("fo", "Faroese"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gl", "Galician"),
    ("ka", "Georgian"),
    ("de", "German"),
    ("el", "Greek"),
    ("gu", "Gujarati"),
    ("ht", "Haitian Creole"),
    ("ha", "Hausa"),
    ("haw", "Hawaiian"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("is", "Icelandic"),
    ("id", "Indonesian"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("jw", "Javanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("km", "Khmer"),
    ("ko", "Korean"),
    ("lo", "Lao"),
    ("la", "Latin"),
    ("lv", "Latvian"),
    ("ln", "Lingala"),
    ("lt", "Lithuanian"),
    ("lb", "Luxembourgish"),
    ("mk", "Macedonian"),
    ("mg", "Malagasy"),
    ("ms", "Malay"),
    ("ml", "Malayalam"),
    ("mt", "Maltese"),
    ("mi", "Maori"),
    ("mr", "Marathi"),
    ("mn", "Mongolian"),
    ("ne", "Nepali"),
    ("no", "Norwegian"),
    ("nn", "Nynorsk"),
    ("oc", "Occitan"),
    ("ps", "Pashto"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("pa", "Punjabi"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sa", "Sanskrit"),
    ("sr", "Serbian"),
    ("sn", "Shona"),
    ("sd", "Sindhi"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("so", "Somali"),
    ("es", "Spanish"),
    ("su", "Sundanese"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("tg", "Tajik"),
    ("ta", "Tamil"),
    ("tt", "Tatar"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("bo", "Tibetan"),
    ("tr", "Turkish"),
    ("tk", "Turkmen"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("vi", "Vietnamese"),
    ("cy", "Welsh"),
    ("yi", "Yiddish"),
    ("yo", "Yoruba"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    #[test]
    fn automatic_leads_and_saves_no_language() {
        assert_eq!(CHOICES[0], ("", "Automatic"));
        // The picker's setter keeps a picked tag with `value_or_none`, so the
        // empty tag it leads with is what clears the saved language.
        assert_eq!(settings::value_or_none(CHOICES[0].0), None);
    }

    #[test]
    fn every_tag_and_name_is_unique() {
        for (ix, (tag, name)) in CHOICES.iter().enumerate() {
            assert!(!tag.is_empty() || ix == 0, "{name} is not Automatic");
            assert!(!name.is_empty(), "{tag} has no name");
            for (other_tag, other_name) in &CHOICES[..ix] {
                assert_ne!(tag, other_tag);
                assert_ne!(name, other_name);
            }
        }
    }
}
