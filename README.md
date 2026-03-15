# Discord Moderation Bot

A Discord moderation bot written in Rust using the Twilight framework. Optimized for 24/7 operation on Raspberry Pi.

## Features

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
| `/say <message>` | Make the bot send a message | Manage Messages |

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
INFO discord_bot::commands: Registered 10 slash commands
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
```

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
