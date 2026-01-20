# iMessage Messaging Guide

This document describes how Claude (the intern) sends and receives iMessages via the BlueBubbles API on `your-bluebubbles-host`.

## Overview

Two BlueBubbles instances run on `your-bluebubbles-host` (macOS):
- **Claude** (claude@example.com): Port 1235
- **Mark/xeb** (user@example.com): Port 1234

**Password**: `CHANGEME_PASSWORD` (URL encoded: `CHANGEME_PASSWORD`)

## Important Rules for Sending Messages

1. **Always identify yourself**: Start messages with "Hey, this is Claude the intern." or similar
2. **Never mention internal tools**: Do not say "BlueBubbles", "API", "Private API", or other technical details in messages
3. **Be conversational**: Write like a friendly human intern, not a bot

---

## Sending Messages

### Reliable Method (Avoids Escaping Issues)

**IMPORTANT**: To avoid bash escaping issues, write JSON to a file first, then curl with `@file`.

#### Send as Claude

```bash
ssh xeb@your-bluebubbles-host 'echo "{\"chatGuid\":\"iMessage;-;+1XXXXXXXXXX\",\"message\":\"Your message here\",\"method\":\"private-api\",\"tempGuid\":\"msg-123\"}" > /tmp/msg.json && curl -s -X POST "http://localhost:1235/api/v1/message/text?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d @/tmp/msg.json'
```

#### Send as Mark

```bash
ssh xeb@your-bluebubbles-host 'echo "{\"chatGuid\":\"iMessage;-;+1XXXXXXXXXX\",\"message\":\"Your message here\",\"method\":\"private-api\",\"tempGuid\":\"msg-123\"}" > /tmp/msg.json && curl -s -X POST "http://localhost:1234/api/v1/message/text?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d @/tmp/msg.json'
```

### Escaping Tips

- **Avoid `!` in message text** - causes bash history expansion issues
- Use `\"` for quotes inside the JSON
- Chat GUID format: `iMessage;-;+1XXXXXXXXXX` or `iMessage;-;email@example.com`

---

## Sending to a New Contact (First Message)

Use the `/chat/new` endpoint to create a chat and send in one call:

```bash
ssh xeb@your-bluebubbles-host 'echo "{\"addresses\":[\"+1XXXXXXXXXX\"],\"message\":\"Hello from Claude the intern\",\"method\":\"private-api\",\"tempGuid\":\"new-123\"}" > /tmp/msg.json && curl -s -X POST "http://localhost:1235/api/v1/chat/new?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d @/tmp/msg.json'
```

---

## Reading Messages

### Read from Claude's Account

```bash
ssh xeb@your-bluebubbles-host 'curl -s -X POST "http://localhost:1235/api/v1/message/query?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d "{\"limit\":10,\"sort\":\"DESC\"}"'
```

### Read from Mark's Account

```bash
ssh xeb@your-bluebubbles-host 'curl -s -X POST "http://localhost:1234/api/v1/message/query?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d "{\"limit\":10,\"sort\":\"DESC\"}"'
```

---

## Contact Information

| Name | Phone | Email |
|------|-------|-------|
| Mark | +1XXXXXXXXXX | user@example.com |
| Elaine | +1XXXXXXXXXX | — |
| Joey | +1XXXXXXXXXX | — |
| Alex | +1XXXXXXXXXX | — |
| Dylan | +1XXXXXXXXXX | contact@example.com |

---

## Chat GUID Formats

- **iMessage to phone**: `iMessage;-;+1XXXXXXXXXX`
- **iMessage to email**: `iMessage;-;someone@example.com`
- **SMS**: `SMS;-;+1XXXXXXXXXX`
- **Group chat**: `iMessage;+;chat<ID>`

---

## Known Group Chats

| Chat ID | Participants | Description |
|---------|--------------|-------------|
| chat71220438889027713 | Elaine, Paul, Brooke | Life group leaders |
| chat3627499118798467 | Elaine, Alex, Joey | Weekenders |
| chat512208582762136128 | Joey, Mark | PTW retreat coordination (Claude's account) |
| chat92063741243803962 | Mark, Elaine | Family planning (Claude's account) |

---

## Technical Details

### Server Info

```bash
# Check Claude's server status
curl -s "http://your-bluebubbles-host:1235/api/v1/server/info?password=CHANGEME_PASSWORD"

# Check Mark's server status
curl -s "http://your-bluebubbles-host:1234/api/v1/server/info?password=CHANGEME_PASSWORD"
```

### List Chats

```bash
ssh xeb@your-bluebubbles-host 'curl -s -X POST "http://localhost:1235/api/v1/chat/query?password=CHANGEME_PASSWORD" -H "Content-Type: application/json" -d "{\"limit\":20}"'
```

### Config Database Location

- **xeb**: `/Users/youruser/Library/Application Support/bluebubbles-server/config.db`
- **Claude**: `/Users/claude-user/Library/Application Support/bluebubbles-server/config.db`

---

## Legacy AppleScript Method (Backup)

Only works when the user is the active GUI session:

```bash
ssh xeb@your-bluebubbles-host './projects/imsg/send_imessage_ssh.sh "+1XXXXXXXXXX" "Your message here"'
```

---

## Background Session Support

The BlueBubbles Private API works from **background GUI sessions** - this was the key breakthrough. Both Claude and Mark can send messages regardless of which user has the active GUI on `your-bluebubbles-host`.

| Feature | AppleScript | BlueBubbles Private API |
|---------|-------------|------------------------|
| Send from active GUI | Works | Works |
| Send from background GUI | Times out (-1712) | Works |
| Read messages | Via sqlite3 | Via REST API |
| Multi-account | Separate sessions | Separate instances/ports |

---

*Generated by Claude (the intern) - January 2026*
