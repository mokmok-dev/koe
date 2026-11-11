// C ABI shim around the macOS 26 `SpeechAnalyzer` API.
//
// Status codes and the strings contract are documented in
// shim/include/koe_speech.h. Every C-callable entry runs off the analyzer's
// concurrency; the mailbox state they read is guarded by one NSLock.

import AVFAudio
import CoreMedia
import Dispatch
import Darwin
import Foundation
import Speech

/// Result of the asynchronous engine setup.
@available(macOS 26.0, *)
private struct SetupOutcome {
    let status: Int32
    let engine: SpeechEngine?
}

/// Slot the synchronous `create` hands the setup task's result to.
@available(macOS 26.0, *)
private final class SetupBox: @unchecked Sendable {
    private let lock = NSLock()
    private var outcome: SetupOutcome?

    func set(_ outcome: SetupOutcome) {
        lock.lock()
        self.outcome = outcome
        lock.unlock()
    }

    func get() -> SetupOutcome? {
        lock.lock()
        defer { lock.unlock() }
        return outcome
    }
}

@available(macOS 26.0, *)
private final class SpeechEngine: @unchecked Sendable {
    // `-swift-version 5` keeps the shared mutable state below compiling; every
    // field it guards is only read or written under `lock`.
    private let lock = NSLock()

    // Mailbox state, guarded by `lock`.
    private var committed: [(text: String, startMs: Int64, endMs: Int64)] = []
    private var volatileTail = ""
    private var volatileRevision: UInt32 = 0
    private var takenVolatileRevision: UInt32 = 0
    private var error: Int32 = 0
    private var handedCommitted: UnsafeMutablePointer<CChar>?
    private var handedVolatile: UnsafeMutablePointer<CChar>?
    private var finished = false
    private var analyzeDone = false
    private var resultDone = false

    let analyzer: SpeechAnalyzer
    let transcriber: SpeechTranscriber
    let inputSequence: AsyncStream<AnalyzerInput>
    let continuation: AsyncStream<AnalyzerInput>.Continuation
    let sourceFormat: AVAudioFormat
    /// nil when the analyzer already accepts the 16 kHz mono source format.
    let converter: AVAudioConverter?
    private let analyzeSem = DispatchSemaphore(value: 0)
    private let resultSem = DispatchSemaphore(value: 0)
    private var analyzeTask: Task<Void, Never>?
    private var resultTask: Task<Void, Never>?

    /// Longest wait for setup, for a finish, and for teardown.
    private static let waitTimeout: TimeInterval = 15
    private static let teardownTimeout: TimeInterval = 5

    init(
        analyzer: SpeechAnalyzer,
        transcriber: SpeechTranscriber,
        inputSequence: AsyncStream<AnalyzerInput>,
        continuation: AsyncStream<AnalyzerInput>.Continuation,
        sourceFormat: AVAudioFormat,
        converter: AVAudioConverter?
    ) {
        self.analyzer = analyzer
        self.transcriber = transcriber
        self.inputSequence = inputSequence
        self.continuation = continuation
        self.sourceFormat = sourceFormat
        self.converter = converter
    }

    /// Builds everything a session needs, without starting its tasks.
    static func setup(localeTag: String?) async -> SetupOutcome {
        guard SpeechTranscriber.isAvailable else {
            return SetupOutcome(status: -3, engine: nil)
        }
        let requested = localeTag.map { Locale(identifier: $0) } ?? Locale.current
        guard let normalized = await SpeechTranscriber.supportedLocale(equivalentTo: requested) else {
            return SetupOutcome(status: -2, engine: nil)
        }
        let installed = await SpeechTranscriber.installedLocales
        guard installed.contains(normalized) else {
            return SetupOutcome(status: -3, engine: nil)
        }
        // The `.transcription` preset reports finalized phrases only, so a
        // dictation would have nothing to draw until it stopped. Volatile
        // results are the tentative `isFinal == false` ones the HUD shows
        // while the user speaks; the attribute keeps the time codes the
        // committed phrases carry.
        let transcriber = SpeechTranscriber(
            locale: normalized,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults],
            attributeOptions: [.audioTimeRange]
        )
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            // The assets are absent and this backend never downloads.
            return SetupOutcome(status: -3, engine: nil)
        }
        guard let source = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        ) else {
            return SetupOutcome(status: -4, engine: nil)
        }
        var converter: AVAudioConverter?
        let compatible = source.sampleRate == format.sampleRate
            && source.commonFormat == format.commonFormat
            && source.channelCount == format.channelCount
            && source.isInterleaved == format.isInterleaved
        if !compatible {
            guard let built = AVAudioConverter(from: source, to: format) else {
                return SetupOutcome(status: -4, engine: nil)
            }
            converter = built
        }
        let (stream, continuation) = AsyncStream.makeStream(
            of: AnalyzerInput.self,
            bufferingPolicy: .unbounded
        )
        let engine = SpeechEngine(
            analyzer: SpeechAnalyzer(modules: [transcriber]),
            transcriber: transcriber,
            inputSequence: stream,
            continuation: continuation,
            sourceFormat: source,
            converter: converter
        )
        return SetupOutcome(status: 0, engine: engine)
    }

    /// Starts the two long-lived tasks; called once `create` hands out a handle.
    func spawnTasks() {
        // The engine owns its tasks and the tasks own the engine; the cycle
        // breaks when each task ends.
        let analyzer = analyzer
        let stream = inputSequence
        analyzeTask = Task {
            defer { markAnalyzeDone() }
            do {
                // `analyzeSequence` returns when the input stream ends.
                _ = try await analyzer.analyzeSequence(stream)
                try await analyzer.finalizeAndFinishThroughEndOfInput()
            } catch {
                record(-4)
            }
        }
        let transcriber = transcriber
        resultTask = Task {
            defer { markResultsDone() }
            do {
                for try await result in transcriber.results {
                    store(result)
                }
            } catch {
                record(-4)
            }
        }
    }

    /// Ends the input and waits for every result to land in the mailbox.
    func finish() -> Int32 {
        lock.lock()
        finished = true
        let analyzeNeeded = !analyzeDone
        let resultNeeded = !resultDone
        lock.unlock()
        continuation.finish()
        var completed = true
        if analyzeNeeded, analyzeSem.wait(timeout: .now() + Self.waitTimeout) == .timedOut {
            completed = false
        }
        if resultNeeded, resultSem.wait(timeout: .now() + Self.waitTimeout) == .timedOut {
            completed = false
        }
        if !completed {
            record(-4)
            return -4
        }
        return 0
    }

    /// Cancels the session, waits for the tasks, and frees handed-out strings.
    func destroy() {
        lock.lock()
        finished = true
        let analyzeNeeded = !analyzeDone
        let resultNeeded = !resultDone
        lock.unlock()
        continuation.finish()
        analyzeTask?.cancel()
        resultTask?.cancel()
        let analyzer = analyzer
        Task.detached { await analyzer.cancelAndFinishNow() }
        if analyzeNeeded {
            _ = analyzeSem.wait(timeout: .now() + Self.teardownTimeout)
        }
        if resultNeeded {
            _ = resultSem.wait(timeout: .now() + Self.teardownTimeout)
        }
        lock.lock()
        if let string = handedCommitted {
            free(string)
        }
        handedCommitted = nil
        if let string = handedVolatile {
            free(string)
        }
        handedVolatile = nil
        lock.unlock()
    }

    /// Converts and yields `frames` samples of 16 kHz mono audio.
    func feed(_ pcm: UnsafePointer<Float>, _ frames: UInt32) -> Int32 {
        lock.lock()
        let closed = finished
        lock.unlock()
        if closed {
            return -5
        }
        let count = Int(frames)
        if count == 0 {
            return 0
        }
        // The analyzer reads buffers after yield returns, so nothing reused
        // may be handed downstream; every buffer below is freshly allocated.
        guard let buffer = AVAudioPCMBuffer(pcmFormat: sourceFormat, frameCapacity: frames) else {
            record(-4)
            return -4
        }
        guard let channel = buffer.floatChannelData?.pointee else {
            record(-4)
            return -4
        }
        channel.update(from: pcm, count: count)
        buffer.frameLength = frames
        guard let converter else {
            // The analyzer format already matches the source.
            return enqueue(buffer)
        }
        return convert(buffer, with: converter)
    }

    /// Pops one committed phrase.
    func takeCommitted(
        text: UnsafeMutablePointer<koe_stt_string>,
        startMs: UnsafeMutablePointer<Int64>,
        endMs: UnsafeMutablePointer<Int64>
    ) -> Int32 {
        lock.lock()
        defer { lock.unlock() }
        if let phrase = committed.first {
            committed.removeFirst()
            startMs.pointee = phrase.startMs
            endMs.pointee = phrase.endMs
            hand(&handedCommitted, phrase.text, text)
            return 1
        }
        if error != 0 {
            return error
        }
        clear(text)
        return 0
    }

    /// Returns the volatile tail when it changed since the previous take.
    func takeVolatile(
        text: UnsafeMutablePointer<koe_stt_string>,
        revision: UnsafeMutablePointer<UInt32>
    ) -> Int32 {
        lock.lock()
        defer { lock.unlock() }
        revision.pointee = volatileRevision
        if volatileRevision != takenVolatileRevision {
            takenVolatileRevision = volatileRevision
            hand(&handedVolatile, volatileTail, text)
            return 1
        }
        if error != 0 {
            return error
        }
        clear(text)
        return 0
    }

    private func markAnalyzeDone() {
        lock.lock()
        analyzeDone = true
        lock.unlock()
        analyzeSem.signal()
    }

    private func markResultsDone() {
        lock.lock()
        resultDone = true
        lock.unlock()
        resultSem.signal()
    }

    private func record(_ status: Int32) {
        lock.lock()
        defer { lock.unlock() }
        if error == 0 {
            error = status
        }
    }

    /// Stores one transcriber result in the mailbox, in engine order.
    private func store(_ result: SpeechTranscriber.Result) {
        let text = String(result.text.characters)
        lock.lock()
        defer { lock.unlock() }
        if result.isFinal {
            committed.append((
                text,
                Self.milliseconds(result.range.start),
                Self.milliseconds(CMTimeRangeGetEnd(result.range))
            ))
            // A final supersedes the volatile previews of its own phrase; the
            // tail must not outlive the phrase it previews.
            if !volatileTail.isEmpty {
                volatileTail = ""
                volatileRevision &+= 1
            }
        } else {
            volatileTail = text
            volatileRevision &+= 1
        }
    }

    /// Milliseconds for a `CMTime` on the analyzer's timeline.
    private static func milliseconds(_ time: CMTime) -> Int64 {
        let seconds = CMTimeGetSeconds(time)
        // Invalid times make CMTimeGetSeconds return NaN, which traps when
        // converted to Int64.
        guard seconds.isFinite else {
            return 0
        }
        return Int64((seconds * 1_000).rounded())
    }

    private func enqueue(_ buffer: AVAudioPCMBuffer) -> Int32 {
        if case .terminated = continuation.yield(AnalyzerInput(buffer: buffer)) {
            record(-4)
            return -4
        }
        return 0
    }

    private func convert(_ input: AVAudioPCMBuffer, with converter: AVAudioConverter) -> Int32 {
        let count = Int(input.frameLength)
        let samples = UnsafeMutablePointer<Float>.allocate(capacity: count)
        defer { samples.deallocate() }
        guard let inChannel = input.floatChannelData?.pointee else {
            record(-4)
            return -4
        }
        samples.update(from: inChannel, count: count)
        let format = sourceFormat
        var offset = 0
        var failed = false
        let block: AVAudioConverterInputBlock = { capacity, status in
            let frames = min(Int(capacity), count - offset)
            if frames <= 0 {
                status.pointee = .noDataNow
                return nil
            }
            guard let chunk = AVAudioPCMBuffer(
                pcmFormat: format,
                frameCapacity: AVAudioFrameCount(frames)
            ) else {
                failed = true
                status.pointee = .noDataNow
                return nil
            }
            guard let channel = chunk.floatChannelData?.pointee else {
                failed = true
                status.pointee = .noDataNow
                return nil
            }
            channel.update(from: samples.advanced(by: offset), count: frames)
            chunk.frameLength = AVAudioFrameCount(frames)
            offset += frames
            status.pointee = .haveData
            return chunk
        }
        let ratio = converter.outputFormat.sampleRate / sourceFormat.sampleRate
        // One call may not drain the input; the loop ends when the block runs
        // dry.
        while !failed {
            let capacity = AVAudioFrameCount(Double(count - offset) * ratio) + 64
            guard let out = AVAudioPCMBuffer(
                pcmFormat: converter.outputFormat,
                frameCapacity: capacity
            ) else {
                record(-4)
                return -4
            }
            var conversionError: NSError?
            let status = converter.convert(to: out, error: &conversionError, withInputFrom: block)
            if out.frameLength > 0 {
                let code = enqueue(out)
                if code != 0 {
                    return code
                }
            }
            switch status {
            case .haveData:
                // Data reported without produced frames cannot make progress.
                if out.frameLength == 0 {
                    return failed ? -4 : 0
                }
                continue
            case .inputRanDry, .endOfStream, .error:
                if failed || status == .error {
                    record(-4)
                    return -4
                }
                return 0
            @unknown default:
                return failed ? -4 : 0
            }
        }
        record(-4)
        return -4
    }

    /// Publishes `value` through `text`, replacing the previous handed-out copy.
    private func hand(
        _ slot: inout UnsafeMutablePointer<CChar>?,
        _ value: String,
        _ text: UnsafeMutablePointer<koe_stt_string>
    ) {
        if let previous = slot {
            free(previous)
        }
        let copy = strdup(value)
        slot = copy
        // A nil copy means the process is out of memory; hand back an empty
        // phrase rather than crash the caller.
        text.pointee = koe_stt_string(
            data: copy,
            length: copy == nil ? 0 : UInt32(clamping: value.utf8.count)
        )
    }

    private func clear(_ text: UnsafeMutablePointer<koe_stt_string>) {
        text.pointee = koe_stt_string(data: nil, length: 0)
    }
}

@_cdecl("koe_stt_engine_create")
func koeSpeechEngineCreate(
    locale: UnsafePointer<CChar>?,
    out: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> Int32 {
    guard let out else {
        return -5
    }
    guard #available(macOS 26.0, *) else {
        return -1
    }
    let localeTag = locale.map { String(cString: $0) }
    let semaphore = DispatchSemaphore(value: 0)
    let box = SetupBox()
    Task.detached {
        box.set(await SpeechEngine.setup(localeTag: localeTag))
        semaphore.signal()
    }
    if semaphore.wait(timeout: .now() + 15) == .timedOut {
        return -4
    }
    guard let engine = box.get()?.engine else {
        return box.get()?.status ?? -4
    }
    engine.spawnTasks()
    out.pointee = Unmanaged.passRetained(engine).toOpaque()
    return 0
}

@_cdecl("koe_stt_engine_feed")
func koeSpeechEngineFeed(
    engine: UnsafeMutableRawPointer?,
    pcm: UnsafePointer<Float>?,
    frames: UInt32
) -> Int32 {
    guard #available(macOS 26.0, *) else {
        return -1
    }
    guard let engine, let pcm else {
        return -5
    }
    return Unmanaged<SpeechEngine>.fromOpaque(engine)
        .takeUnretainedValue()
        .feed(pcm, frames)
}

@_cdecl("koe_stt_engine_take_committed")
func koeSpeechEngineTakeCommitted(
    engine: UnsafeMutableRawPointer?,
    text: UnsafeMutablePointer<koe_stt_string>?,
    startMs: UnsafeMutablePointer<Int64>?,
    endMs: UnsafeMutablePointer<Int64>?
) -> Int32 {
    guard #available(macOS 26.0, *) else {
        return -1
    }
    guard let engine, let text, let startMs, let endMs else {
        return -5
    }
    return Unmanaged<SpeechEngine>.fromOpaque(engine)
        .takeUnretainedValue()
        .takeCommitted(text: text, startMs: startMs, endMs: endMs)
}

@_cdecl("koe_stt_engine_take_volatile")
func koeSpeechEngineTakeVolatile(
    engine: UnsafeMutableRawPointer?,
    text: UnsafeMutablePointer<koe_stt_string>?,
    revision: UnsafeMutablePointer<UInt32>?
) -> Int32 {
    guard #available(macOS 26.0, *) else {
        return -1
    }
    guard let engine, let text, let revision else {
        return -5
    }
    return Unmanaged<SpeechEngine>.fromOpaque(engine)
        .takeUnretainedValue()
        .takeVolatile(text: text, revision: revision)
}

@_cdecl("koe_stt_engine_finish")
func koeSpeechEngineFinish(engine: UnsafeMutableRawPointer?) -> Int32 {
    guard #available(macOS 26.0, *) else {
        return -1
    }
    guard let engine else {
        return -5
    }
    return Unmanaged<SpeechEngine>.fromOpaque(engine).takeUnretainedValue().finish()
}

@_cdecl("koe_stt_engine_destroy")
func koeSpeechEngineDestroy(engine: UnsafeMutableRawPointer?) {
    guard #available(macOS 26.0, *), let engine else {
        return
    }
    // Taking the retained value keeps the engine alive until `destroy` ends.
    Unmanaged<SpeechEngine>.fromOpaque(engine).takeRetainedValue().destroy()
}
