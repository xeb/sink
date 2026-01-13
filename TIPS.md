# Building Master Sink on macOS

## Prerequisites

### Xcode
- Install Xcode 15+ from the Mac App Store
- Open Xcode at least once to accept the license agreement
- Install command line tools: `xcode-select --install`

### Apple Developer Account
- Required for running on a physical iOS device
- Free account works for development, but you'll need to re-sign every 7 days
- Paid account ($99/year) required for push notifications and TestFlight

## Project Setup

### 1. Create the Xcode Project

```bash
# Clone or navigate to the project directory
cd /path/to/sink

# Open Xcode and create new project:
# - iOS > App
# - Product Name: MasterSink
# - Interface: SwiftUI
# - Language: Swift
# - Bundle Identifier: com.yourname.mastersink
```

### 2. Add Citadel Dependency

In Xcode:
1. File > Add Package Dependencies
2. Enter: `https://github.com/orlandos-nl/Citadel.git`
3. Set version rule: "Up to Next Major" from `0.7.0`
4. Add to MasterSink target

Or add to `Package.swift` if using SPM:
```swift
dependencies: [
    .package(url: "https://github.com/orlandos-nl/Citadel.git", from: "0.7.0")
]
```

### 3. Configure Capabilities

In Xcode, select the MasterSink target > Signing & Capabilities:

1. **+ Capability > Background Modes**
   - Check "Audio, AirPlay, and Picture in Picture" (for TTS)
   - Check "Background fetch" (for polling Claude responses)

2. **+ Capability > Push Notifications** (if using APNs)

3. **+ Capability > Keychain Sharing** (optional, for SSH key storage)

### 4. Add Info.plist Entries

Add these keys to Info.plist (or via Xcode's Info tab):

```xml
<key>NSSpeechRecognitionUsageDescription</key>
<string>Master Sink uses speech recognition to send voice commands to Claude.</string>

<key>NSMicrophoneUsageDescription</key>
<string>Master Sink needs microphone access to hear your voice commands.</string>
```

## Building & Running

### Simulator
- Select an iPhone simulator from the device dropdown
- Press Cmd+R to build and run
- Note: Speech recognition works in simulator but may be less reliable

### Physical Device
1. Connect your iPhone via USB
2. Select your device from the dropdown
3. First time: Trust the computer on your iPhone
4. First time: Go to Settings > General > VPN & Device Management and trust your developer certificate
5. Press Cmd+R to build and run

## Common Issues

### "Citadel failed to build"

Citadel requires iOS 15+ and uses SwiftNIO. If you see build errors:

```bash
# Clean derived data
rm -rf ~/Library/Developer/Xcode/DerivedData

# Reset package cache
File > Packages > Reset Package Caches
```

### "Speech recognition not available"

- Ensure Siri is enabled on the device (Settings > Siri & Search)
- Check that the device has an internet connection (first use downloads the model)
- On-device recognition requires iOS 13+ and A12 chip or newer

### "SSH connection timeout"

- Verify the target machine is reachable: `ping <hostname>`
- Check SSH is running on target: `ssh user@host`
- Ensure your SSH key is in the correct format (Ed25519 or RSA)
- Check firewall allows port 22 (or your custom port)

### "tmux session not found"

On the target machine, ensure the tmux session exists:
```bash
# Create session if it doesn't exist
tmux new-session -d -s main -n claude

# Verify session
tmux list-sessions
```

### Code Signing Issues

```bash
# If you see "codesign failed":
# 1. Check you're signed into Xcode with your Apple ID
# 2. Select your Team in Signing & Capabilities
# 3. Let Xcode manage signing automatically
```

## Testing Speech Recognition

Speech recognition requires user permission and works best with:
- A quiet environment
- Clear pronunciation
- Short pauses between phrases

Test the silence detection threshold (1.5s default) and adjust in `SilenceDetector.swift` if needed.

## Debugging SSH

Enable verbose logging for SSH connections:

```swift
// In SSHManager.swift, before connecting:
import Logging
LoggingSystem.bootstrap { label in
    var handler = StreamLogHandler.standardOutput(label: label)
    handler.logLevel = .debug
    return handler
}
```

## Gemini API Key

1. Get an API key from [Google AI Studio](https://aistudio.google.com/app/apikey)
2. Store it securely (don't commit to git)
3. For development, you can use a `.xcconfig` file:

```
// Debug.xcconfig
GEMINI_API_KEY = your-api-key-here
```

Then access in code:
```swift
let apiKey = Bundle.main.infoDictionary?["GEMINI_API_KEY"] as? String ?? ""
```

## Performance Tips

- Use on-device speech recognition (`requiresOnDeviceRecognition = true`) for better privacy and lower latency
- Cache tmux captures to avoid excessive SSH round trips
- Implement response diffing to only summarize new content
- Use `AVSpeechUtteranceDefaultSpeechRate` or slightly faster for voice output

## Device-Specific Notes

### iPhone
- Works with built-in mic, AirPods, or other Bluetooth headsets
- Half-duplex constraint critical to prevent TTS from triggering STT

### iPad
- Same functionality as iPhone
- Consider landscape layout for larger screens

### Mac Catalyst (Optional)
- Project can be built for Mac Catalyst with minor modifications
- Useful for testing without a physical iOS device
