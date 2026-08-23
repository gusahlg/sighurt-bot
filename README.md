# Discord Moderation Bot

A Discord moderation + AI chat bot written in Rust using the Twilight
framework. Originally built for a Raspberry Pi; since 2026-07-24 it runs
on the desktop next to the SuperSighurt LLM it chats through (see
`docs/discord-ai-setup.md` in the `artificial-stupidity` repo for the
full single-machine architecture).

## Features

### AI Chat (SuperSighurt)
- Answers **DMs and @-mentions** by calling the local LLM server
  (`[chat] endpoint_url`, X-API-Key auth). `!ai on|off|status` toggles it
  at runtime for the configured admins.
- **Understands Discord replies**: when someone replies to a message
  (the bot's or anyone's), the replied-to author + text are sent to the
  LLM as context, and the bot's answer is posted as a real Discord reply.
- **Understands the ambient conversation**: every trigger also carries the
  newest structured channel messages (oldest first), including authors and
  reply links, rather than presenting an isolated @-mention to the model.
- **Searches the live web on explicit request**: `!search QUERY`, "search the
  web for ...", and "look up ..." retrieve bounded HTTPS results before the
  LLM call. Snippets are labelled as untrusted evidence and the bot appends the
  provider URLs itself. `BRAVE_SEARCH_API_KEY` enables Brave; a zero-key
  DuckDuckGo/Wikimedia fallback is built in.
- **Talks to other bots** (`[chat] respond_to_bots`), with a
  `max_bot_chain` loop guard (default 3 consecutive bot-triggered
  replies per channel; any human message resets it) so two bots can't
  ping-pong forever.
- **Proper pings**: incoming `<@id>` mentions are converted to readable
  `@name` for the model; `@name` in the model's output is resolved back
  to a real `<@id>` ping. Everything else is ping-suppressed via a
  strict `allowed_mentions` (the global default suppresses ALL pings —
  no accidental `@everyone`).

### Training-data capture
- Logs **every message in every server** (humans and bots) to
  `data/channels/<guild|dm>/<channel>.tsv` — message id, timestamp,
  author, display name, **reply-to id**, content.
- **Backfill + forward catch-up**: on startup (+30s) and every
  `[scrape] interval_hours` (default 24), the bot backfills unseen
  history and pages forward past its per-channel cursors, healing any
  offline gap. Threads and forum posts are covered (active + archived
  public threads). `cargo run --bin scraper` still works standalone.
- The `artificial-stupidity` repo converts these TSVs into the LLM's
  training corpus weekly (reply-chains stitched into clean dialogs,
  mentions as learnable `@name` tokens).

### Moderation Commands
| Command | Description | Permission |
|---------|-------------|------------|
| `/ban <user> [reason] [delete_days]` | Ban a user from the server | Ban Members |
| `/kick <user> [reason]` | Kick a user from the server | Kick Members |
| `/mute <user> <duration> [reason]` | Timeout a user (duration in minutes) | Moderate Members |
| `/purge <count> [user]` | Delete messages in bulk | Manage Messages |

### Utility Commands
| Command | Description | Permission |
|---------|-------------|------------|
| `/ping` | Check if the bot is responsive | Everyone |
| `/userinfo [user]` | Get information about a user | Everyone |
| `/serverinfo` | Get information about the server | Everyone |
| `/say <message>` | Temporarily disabled during AI testing | Manage Messages |

### Auto-Moderation
| Command | Description | Permission |
|---------|-------------|------------|
| `/automod spam <on/off>` | Toggle spam detection | Administrator |
| `/automod raid <on/off>` | Toggle raid protection | Administrator |
| `/automod words add/remove/list` | Manage word filter | Administrator |

### Auto-Role
| Command | Description | Permission |
|---------|-------------|------------|
| `/autorole set <role>` | Auto-assign role to new members | Administrator |
| `/autorole off` | Disable auto-role | Administrator |
| `/autorole status` | Check current auto-role setting | Administrator |

---

## Quick Start

### 1. Create a Discord Bot

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
2. Click **"New Application"** and give it a name
3. Go to the **"Bot"** section in the left sidebar
4. Click **"Reset Token"** and copy the token (save it securely!)
5. Enable these **Privileged Gateway Intents**:
   - ✅ Server Members Intent
   - ✅ Message Content Intent
6. Go to **"OAuth2" → "URL Generator"**
7. Select scopes: `bot`, `applications.commands`
8. Select permissions: `Administrator` (or specific permissions below)
9. Copy the generated URL and open it to invite the bot to your server

**Minimum Required Permissions:**
- Kick Members, Ban Members, Moderate Members
- Manage Messages, Read Message History
- Manage Roles (for auto-role feature)

### 2. Get Your Application ID

In the Discord Developer Portal, go to **"General Information"** and copy the **Application ID**.

---

## Raspberry Pi Deployment

### Prerequisites

- Raspberry Pi 3, 4, or 5 (64-bit OS recommended)
- Raspberry Pi OS (64-bit) or Ubuntu Server for ARM64
- Internet connection

### Option A: Use Pre-built Binary (Recommended)

The release binary is pre-compiled for ARM64 (aarch64). This works on:
- Raspberry Pi 3/4/5 with 64-bit OS
- Raspberry Pi Zero 2 W with 64-bit OS

#### Step 1: Transfer Files to Raspberry Pi

From your build machine:
```bash
# Create a deployment package
mkdir -p deploy
cp target/aarch64-unknown-linux-gnu/release/discord-bot deploy/
cp .env.example deploy/.env
cp discord-bot.service deploy/

# Transfer to Raspberry Pi (replace with your Pi's IP)
scp -r deploy/* pi@raspberrypi.local:~/discord-bot/
```

Or on the Raspberry Pi, download directly:
```bash
mkdir -p ~/discord-bot
cd ~/discord-bot
# Copy the binary from your build machine or download from releases
```

#### Step 2: Configure the Bot

SSH into your Raspberry Pi:
```bash
ssh pi@raspberrypi.local
cd ~/discord-bot
```

Edit the `.env` file:
```bash
nano .env
```

Set your credentials:
```env
DISCORD_TOKEN=your_bot_token_here
DISCORD_APPLICATION_ID=your_application_id_here
DATABASE_URL=sqlite:data/bot.db
RUST_LOG=info
```

#### Step 3: Test the Bot

```bash
# Make executable
chmod +x discord-bot

# Create data directory
mkdir -p data

# Run the bot
./discord-bot
```

You should see:
```
INFO discord_bot: Configuration loaded
INFO discord_bot::database: Database initialized successfully
INFO discord_bot::commands: Registered 9 slash commands
INFO discord_bot::events: Bot is ready! Logged in as YourBot#1234
```

Press `Ctrl+C` to stop.

#### Step 4: Install as a System Service

```bash
# Create a system user for the bot
sudo useradd -r -s /bin/false discord-bot

# Create installation directory
sudo mkdir -p /opt/discord-bot/data
sudo cp discord-bot /opt/discord-bot/
sudo cp .env /opt/discord-bot/
sudo chown -R discord-bot:discord-bot /opt/discord-bot
sudo chmod 600 /opt/discord-bot/.env

# Install the systemd service
sudo cp discord-bot.service /etc/systemd/system/

# Reload systemd and enable the service
sudo systemctl daemon-reload
sudo systemctl enable discord-bot
sudo systemctl start discord-bot
```

#### Step 5: Verify It's Running

```bash
# Check status
sudo systemctl status discord-bot

# View logs
sudo journalctl -u discord-bot -f

# Restart if needed
sudo systemctl restart discord-bot
```

---

### Option B: Build on Raspberry Pi

If you prefer to compile directly on the Pi:

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install build dependencies
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev

# Clone and build (this will take 10-30 minutes on Pi 4)
git clone <your-repo-url> discord-bot
cd discord-bot
cargo build --release

# The binary will be at target/release/discord-bot
```

---

### Option C: Cross-Compile from x86_64 Linux

On your development machine:

```bash
# Install the ARM64 target
rustup target add aarch64-unknown-linux-gnu

# Install cross-compiler (Arch Linux)
sudo pacman -S aarch64-linux-gnu-gcc

# Or on Ubuntu/Debian
sudo apt install gcc-aarch64-linux-gnu

# Create .cargo/config.toml
mkdir -p .cargo
cat > .cargo/config.toml << 'EOF'
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
EOF

# Build
cargo build --release --target aarch64-unknown-linux-gnu

# Binary is at: target/aarch64-unknown-linux-gnu/release/discord-bot
```

---

## Configuration

### Environment Variables (.env)

| Variable | Description | Required |
|----------|-------------|----------|
| `DISCORD_TOKEN` | Bot token from Discord Developer Portal | Yes |
| `DISCORD_APPLICATION_ID` | Application ID from Developer Portal | Yes |
| `DATABASE_URL` | SQLite database path (default: `sqlite:data/bot.db`) | No |
| `RUST_LOG` | Log level: `trace`, `debug`, `info`, `warn`, `error` | No |
| `LLM_API_KEY` | Shared secret for the SuperSighurt LLM server (enables chat) | For chat |
| `BRAVE_SEARCH_API_KEY` | Optional broad web-search provider; zero-key fallback is automatic | No |
| `ELEVENLABS_AGENT_ID` / `ELEVENLABS_API_KEY` | Voice bridge (optional) | For voice |

### Optional: config.toml

Create `config.toml` for default settings:

```toml
[automod]
spam_enabled = true
spam_threshold = 5      # Messages per interval to trigger
spam_interval = 5       # Interval in seconds

raid_enabled = true
raid_threshold = 10     # Joins per interval to trigger
raid_interval = 10      # Interval in seconds

[moderation]
default_reason = "No reason provided"
default_delete_days = 1

[chat]
enabled = true                    # boot state of the LLM chat (!ai toggles at runtime)
endpoint_url = "http://127.0.0.1:8088"
request_timeout_secs = 90          # server allows 75s; leave client/transport margin
admin_user_ids = [123456789012345678]
respond_to_bots = true            # answer other bots' mentions/DMs
max_bot_chain = 3                 # consecutive bot-triggered replies per channel before going quiet
reply_context_max_chars = 300     # replied-to message excerpt sent to the LLM
recent_context_messages = 12      # ambient channel messages, oldest first
context_message_max_chars = 400   # per ambient-message cap
web_search_enabled = true         # explicit live retrieval only
web_search_max_results = 4        # bounded untrusted evidence snippets
web_search_timeout_secs = 12

[scrape]
enabled = true                    # in-process backfill + catch-up scraper
interval_hours = 24               # cadence after the ~30s post-boot run
```

The deployed user service restarts the local model first and uses
`scripts/wait_for_llm.sh` to wait for `/healthz` before opening the Discord
gateway. The wait is non-fatal, so moderation and archive capture still start
if model inference is temporarily unavailable.

---

## Database

The bot uses SQLite for persistence. The database is created automatically at the path specified in `DATABASE_URL`.

**Tables:**
- `mod_logs` - Moderation action history
- `filtered_words` - Per-guild word filter
- `guild_settings` - Per-guild configuration (spam, raid, autorole)

**Backup:**
```bash
# The database is a single file
cp /opt/discord-bot/data/bot.db /backup/bot.db.$(date +%Y%m%d)
```

---

## Updating the Bot

```bash
# Stop the service
sudo systemctl stop discord-bot

# Replace the binary
sudo cp new-discord-bot /opt/discord-bot/discord-bot
sudo chown discord-bot:discord-bot /opt/discord-bot/discord-bot

# Start the service
sudo systemctl start discord-bot

# Check logs
sudo journalctl -u discord-bot -f
```

---

## Troubleshooting

### Bot won't start

1. Check the logs: `sudo journalctl -u discord-bot -n 50`
2. Verify `.env` file has correct token and application ID
3. Ensure the bot has been invited to your server

### Commands not showing up

- Slash commands can take up to 1 hour to propagate globally
- Try kicking and re-inviting the bot
- Check that `applications.commands` scope was selected when inviting

### Permission errors

- Ensure the bot's role is higher than roles it needs to manage
- Check that required permissions are granted

### Database errors

```bash
# Reset the database (warning: loses all data)
sudo systemctl stop discord-bot
sudo rm /opt/discord-bot/data/bot.db
sudo systemctl start discord-bot
```

---

## Resource Usage

On Raspberry Pi 4:
- **Memory:** ~30-50 MB
- **CPU:** <1% idle, spikes during activity
- **Storage:** ~15 MB (binary + database)

The release build is optimized with LTO and stripped symbols for minimal size.

---

## License

MIT
