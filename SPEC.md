# Master Sink - Implementation Specification

## Overview

Master Sink is a voice-first iOS app for interacting with Claude Code via SSH/tmux. It uses native iOS speech recognition, Gemini for response summarization, and iOS TTS for audio output.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                         iOS App (Master Sink)                     │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────┐  │
│  │   Voice     │    │   State     │    │   Response          │  │
│  │   Manager   │───►│   Machine   │───►│   Handler           │  │
│  │ (STT/TTS)   │    │             │    │                     │  │
│  └─────────────┘    └──────┬──────┘    └──────────┬──────────┘  │
│                            │                       │              │
│                            ▼                       ▼              │
│                    ┌─────────────┐         ┌─────────────┐       │
│                    │   SSH       │         │   Gemini    │       │
│                    │   Client    │         │   API       │       │
│                    │  (Citadel)  │         │ (Summarize) │       │
│                    └──────┬──────┘         └─────────────┘       │
│                           │                                       │
└───────────────────────────┼───────────────────────────────────────┘
                            │ SSH
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                    Target Machine (Linux/Mac)                     │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                     tmux session                             │ │
│  │  ┌─────────────────────────────────────────────────────────┐│ │
│  │  │                   Claude Code                           ││ │
│  │  │                                                         ││ │
│  │  │  send-keys ─────────────────────────────────►           ││ │
│  │  │            ◄───────────────────────────── capture-pane  ││ │
│  │  └─────────────────────────────────────────────────────────┘│ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
```

---

## Tech Stack

| Component | Technology | Notes |
|-----------|------------|-------|
| **UI Framework** | SwiftUI | Modern declarative UI |
| **STT** | Speech.framework (SFSpeechRecognizer) | On-device recognition |
| **TTS** | AVSpeechSynthesizer | Native iOS voice output |
| **SSH Client** | **Citadel** | Pure Swift, async/await, built on SwiftNIO SSH |
| **Summarization** | Gemini API | Condense verbose Claude responses |
| **Notifications** | APNs + Background Tasks | Long-running task alerts |

---

## Voice Interface Design

### State Machine

The app operates as a **Finite State Machine** with strict turn-taking (half-duplex):

```
┌───────┐     tap/wake      ┌───────────┐    silence    ┌────────────┐
│ IDLE  │ ─────────────────►│ LISTENING │ ─────────────►│ PROCESSING │
└───────┘                   └───────────┘               └─────┬──────┘
    ▲                                                         │
    │                                                         ▼
    │         TTS finished      ┌──────────┐      response ready
    └───────────────────────────│ SPEAKING │◄─────────────────┘
                                └──────────┘
```

### States

| State | Microphone | Speaker | Visual Feedback |
|-------|------------|---------|-----------------|
| **IDLE** | Off | Off | Subtle pulse or static icon |
| **LISTENING** | On | Off | Animated waveform |
| **PROCESSING** | Off | Off | Loading spinner |
| **SPEAKING** | Off | On | Audio visualization |

### Critical Constraints

1. **Half-Duplex**: Microphone MUST be disabled during TTS to prevent feedback loop
2. **Silence Detection**: Use 1.5s timer that resets on each partial result
3. **Earcons**: Play short audio cue when transitioning to LISTENING state

---

## Voice Recognition (STT)

### Library: Speech.framework

```swift
import Speech

// Key configuration
let recognizer = SFSpeechRecognizer(locale: Locale(identifier: "en-US"))
recognizer?.supportsOnDeviceRecognition = true  // Privacy + offline

let request = SFSpeechAudioBufferRecognitionRequest()
request.requiresOnDeviceRecognition = true
request.shouldReportPartialResults = true
```

### Required Permissions (Info.plist)

```xml
<key>NSSpeechRecognitionUsageDescription</key>
<string>Master Sink uses speech recognition to send voice commands to Claude.</string>
<key>NSMicrophoneUsageDescription</key>
<string>Master Sink needs microphone access to hear your voice commands.</string>
```

### Audio Session Configuration

```swift
import AVFoundation

let session = AVAudioSession.sharedInstance()
try session.setCategory(
    .playAndRecord,
    mode: .measurement,
    options: [.duckOthers, .defaultToSpeaker, .allowBluetooth]
)
try session.setActive(true, options: .notifyOthersOnDeactivation)
```

### Silence Detection Pattern

```swift
class SilenceDetector {
    private var silenceTimer: Timer?
    private let silenceThreshold: TimeInterval = 1.5

    func onPartialResult(_ text: String) {
        silenceTimer?.invalidate()
        silenceTimer = Timer.scheduledTimer(
            withTimeInterval: silenceThreshold,
            repeats: false
        ) { [weak self] _ in
            self?.onSilenceDetected()
        }
    }

    func onSilenceDetected() {
        // Transition to PROCESSING state
    }
}
```

---

## Text-to-Speech (TTS)

### Library: AVSpeechSynthesizer

```swift
import AVFoundation

class TTSManager: NSObject, AVSpeechSynthesizerDelegate {
    private let synthesizer = AVSpeechSynthesizer()

    func speak(_ text: String) {
        let utterance = AVSpeechUtterance(string: text)
        utterance.voice = AVSpeechSynthesisVoice(language: "en-US")
        utterance.rate = AVSpeechUtteranceDefaultSpeechRate
        utterance.pitchMultiplier = 1.0

        synthesizer.speak(utterance)
    }

    func speechSynthesizer(
        _ synthesizer: AVSpeechSynthesizer,
        didFinish utterance: AVSpeechUtterance
    ) {
        // Transition back to IDLE or LISTENING
    }
}
```

---

## SSH Client

### Library: Citadel

**Why Citadel:**
- Pure Swift (no C bridging headers)
- Built on Apple's SwiftNIO SSH
- Full async/await support
- Active maintenance
- Easy command execution

**Avoid:** NMSSH, Shout, SwiftSH (legacy, unmaintained)

### Installation

```swift
// Package.swift
dependencies: [
    .package(url: "https://github.com/orlandos-nl/Citadel.git", from: "0.7.0")
]
```

### Connection Setup

```swift
import Citadel

class SSHManager {
    private var client: SSHClient?

    func connect(
        host: String,
        port: Int = 22,
        username: String,
        privateKey: String
    ) async throws {
        client = try await SSHClient.connect(
            host: host,
            port: port,
            authenticationMethod: .privateKey(
                username: username,
                privateKey: .init(sshEd25519: privateKey)
            ),
            hostKeyValidator: .acceptAnything()  // TODO: Implement proper validation
        )
    }

    func executeCommand(_ command: String) async throws -> String {
        guard let client else { throw SSHError.notConnected }

        let result = try await client.executeCommand(command)
        return String(buffer: result)
    }
}
```

### tmux Integration

```swift
extension SSHManager {
    /// Send a command to Claude Code via tmux
    func sendToTmux(
        sessionName: String = "main",
        windowName: String = "claude",
        command: String
    ) async throws {
        // Escape the command for tmux
        let escaped = command.replacingOccurrences(of: "'", with: "'\\''")

        let tmuxCmd = "tmux send-keys -t '\(sessionName):\(windowName)' '\(escaped)' Enter"
        _ = try await executeCommand(tmuxCmd)
    }

    /// Capture Claude's response from tmux
    func captureFromTmux(
        sessionName: String = "main",
        windowName: String = "claude",
        lines: Int = 100
    ) async throws -> String {
        let tmuxCmd = "tmux capture-pane -t '\(sessionName):\(windowName)' -p -S -\(lines)"
        return try await executeCommand(tmuxCmd)
    }
}
```

### Key Storage

Store SSH private key in iOS Keychain:

```swift
import Security

class KeychainManager {
    static let shared = KeychainManager()

    func storePrivateKey(_ key: String, for host: String) throws {
        let data = key.data(using: .utf8)!
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrAccount as String: "ssh_key_\(host)",
            kSecValueData as String: data
        ]
        SecItemAdd(query as CFDictionary, nil)
    }

    func retrievePrivateKey(for host: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrAccount as String: "ssh_key_\(host)",
            kSecReturnData as String: true
        ]
        var result: AnyObject?
        SecItemCopyMatching(query as CFDictionary, &result)
        guard let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }
}
```

### SSH Connection String Input

Settings uses a single text field for SSH connection string (familiar format):

```
user@hostname:port
```

**Examples:**
- `xeb@192.168.1.100:22`
- `deploy@myserver.local` (port defaults to 22)
- `root@10.0.0.5:2222`

**Parser:**

```swift
struct SSHConnectionString {
    let username: String
    let host: String
    let port: Int

    init?(from string: String) {
        // Parse: user@host:port or user@host
        let pattern = #"^([^@]+)@([^:]+)(?::(\d+))?$"#
        guard let regex = try? NSRegularExpression(pattern: pattern),
              let match = regex.firstMatch(
                  in: string,
                  range: NSRange(string.startIndex..., in: string)
              ) else {
            return nil
        }

        guard let userRange = Range(match.range(at: 1), in: string),
              let hostRange = Range(match.range(at: 2), in: string) else {
            return nil
        }

        self.username = String(string[userRange])
        self.host = String(string[hostRange])

        if let portRange = Range(match.range(at: 3), in: string) {
            self.port = Int(string[portRange]) ?? 22
        } else {
            self.port = 22
        }
    }

    var displayString: String {
        port == 22 ? "\(username)@\(host)" : "\(username)@\(host):\(port)"
    }
}
```

**Settings UI:**

```swift
struct SettingsView: View {
    @AppStorage("sshConnectionString") private var connectionString = ""
    @State private var isValid = true

    var body: some View {
        Form {
            Section("SSH Connection") {
                TextField("user@host:port", text: $connectionString)
                    .textContentType(.URL)
                    .autocapitalization(.none)
                    .disableAutocorrection(true)
                    .onChange(of: connectionString) { newValue in
                        isValid = SSHConnectionString(from: newValue) != nil
                    }

                if !isValid && !connectionString.isEmpty {
                    Text("Invalid format. Use: user@host:port")
                        .foregroundColor(.red)
                        .font(.caption)
                }
            }

            Section("SSH Key") {
                // Key import UI...
            }
        }
    }
}
```

---

## Gemini Summarization

Claude Code responses can be verbose. Use Gemini to create voice-friendly summaries.

### API Integration

```swift
import Foundation

class GeminiSummarizer {
    private let apiKey: String
    private let endpoint = "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"

    init(apiKey: String) {
        self.apiKey = apiKey
    }

    func summarize(_ text: String, maxWords: Int = 50) async throws -> String {
        let prompt = """
        Summarize the following Claude Code response for voice output.
        Be concise (\(maxWords) words max), conversational, and focus on the key action or result.
        Skip code blocks and technical details unless critical.

        Response to summarize:
        \(text)
        """

        var request = URLRequest(url: URL(string: "\(endpoint)?key=\(apiKey)")!)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode([
            "contents": [["parts": [["text": prompt]]]]
        ])

        let (data, _) = try await URLSession.shared.data(for: request)
        // Parse response and extract text
        // ...
        return summary
    }
}
```

### Summarization Strategy

| Claude Response Type | Summarization Approach |
|---------------------|------------------------|
| Code written | "I created/modified [file]. [1-sentence description]" |
| Search results | "I found [N] matches. The most relevant is [top result]" |
| Error | "There was an error: [brief description]" |
| Question | Pass through directly (Claude is asking for clarification) |
| Long explanation | Extract key points, skip examples |

---

## Master Project Integration

### Reading Project TODOs

```swift
struct MasterProject {
    let sshManager: SSHManager
    let masterPath = "/path/to/working/directory"

    func getHighPriorityTodos() async throws -> [Todo] {
        let command = """
        find \(masterPath)/projects -name "TODO.md" -exec \
            grep -A 10 "## High Priority" {} \\; 2>/dev/null
        """
        let output = try await sshManager.executeCommand(command)
        return parseTodos(output)
    }

    func getProjectStatus(_ project: String) async throws -> String {
        let statusPath = "\(masterPath)/projects/\(project)/STATUS.md"
        return try await sshManager.executeCommand("cat \(statusPath)")
    }
}
```

### Voice Commands

| Voice Command | Action |
|---------------|--------|
| "What's my top priority?" | Read highest priority TODO |
| "List all projects" | Read project names |
| "Status of [project]" | Read STATUS.md for project |
| "Mark [task] as done" | Send to Claude to update TODO.md |
| "Add todo to [project]" | Send to Claude to add TODO |

---

## Push Notifications

For long-running Claude tasks, notify user when complete.

### Setup

1. Enable Push Notifications capability in Xcode
2. Register for remote notifications
3. Implement background task for polling

```swift
import BackgroundTasks
import UserNotifications

class NotificationManager {
    static let shared = NotificationManager()

    func requestPermission() async -> Bool {
        let center = UNUserNotificationCenter.current()
        do {
            return try await center.requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            return false
        }
    }

    func scheduleLocalNotification(title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default

        let request = UNNotificationRequest(
            identifier: UUID().uuidString,
            content: content,
            trigger: nil  // Immediate
        )
        UNUserNotificationCenter.current().add(request)
    }
}
```

---

## Project Structure

```
sink/
├── MasterSink.xcodeproj
├── MasterSink/
│   ├── App/
│   │   ├── MasterSinkApp.swift
│   │   └── ContentView.swift
│   ├── Voice/
│   │   ├── VoiceManager.swift
│   │   ├── SpeechRecognizer.swift
│   │   ├── TTSManager.swift
│   │   └── SilenceDetector.swift
│   ├── SSH/
│   │   ├── SSHManager.swift
│   │   ├── SSHConnectionString.swift
│   │   ├── TmuxController.swift
│   │   └── KeychainManager.swift
│   ├── Claude/
│   │   ├── ClaudeSession.swift
│   │   └── ResponseParser.swift
│   ├── Gemini/
│   │   └── GeminiSummarizer.swift
│   ├── Master/
│   │   ├── MasterProject.swift
│   │   ├── TodoParser.swift
│   │   └── ProjectList.swift
│   ├── Notifications/
│   │   └── NotificationManager.swift
│   ├── UI/
│   │   ├── VoiceButton.swift
│   │   ├── WaveformView.swift
│   │   ├── TodoListView.swift
│   │   └── SettingsView.swift
│   └── Resources/
│       ├── Assets.xcassets
│       │   └── AppIcon (kitchen sink)
│       └── earcon.wav
└── MasterSinkTests/
```

---

## Implementation Phases

### Phase 1: Core Voice Loop
1. SwiftUI app scaffold
2. SFSpeechRecognizer integration
3. AVSpeechSynthesizer integration
4. State machine implementation
5. Basic UI with waveform

### Phase 2: SSH + Claude
1. Citadel SSH integration
2. tmux send-keys/capture-pane
3. Response polling
4. Connection management

### Phase 3: Gemini Summarization
1. Gemini API integration
2. Response classification
3. Voice-friendly formatting

### Phase 4: Master Project
1. TODO parsing
2. Voice commands for TODO management
3. Project dashboard UI

### Phase 5: Polish
1. Push notifications
2. Background processing
3. Settings (voice, host, etc.)
4. Error handling
5. App icon (kitchen sink)

---

## Testing Checklist

- [ ] STT works with AirPods
- [ ] TTS doesn't trigger STT (half-duplex)
- [ ] Silence detection works reliably
- [ ] SSH connects to target machine
- [ ] tmux send-keys delivers command
- [ ] tmux capture-pane retrieves response
- [ ] Gemini summarizes effectively
- [ ] Background reconnection works
- [ ] Notifications fire for long tasks
