# mnemo

`mnemo` records a voice note from your default microphone, transcribes it with
ElevenLabs, and stores the resulting text in Hindsight.

## Requirements

- Rust and Cargo
- A working macOS microphone input
- An ElevenLabs API key with Speech-to-Text access
- A running Hindsight API endpoint

## Configuration

The default config file location is:

```bash
~/.config/mnemo/config.toml
```

Create a starter config:

```bash
mnemo init
```

Or create it from the example:

```bash
mkdir -p ~/.config/mnemo
cp config.example.toml ~/.config/mnemo/config.toml
```

All configuration lives under a named profile. The default profile is
`default`, and it is used when no profile is specified.

Supported config keys:

```toml
[profiles.default]
hindsight_url = "https://your-hindsight-api.example.com"
bank = "personal"
language = "eng"
model = "scribe_v2"
context = "user recorded voice memo"

# Optional Hindsight retain fields. Leave unset to send null.
# metadata = { source = "mnemo" }
# tags = ["voice-note"]
# strategy = "append"

# Defaults to ~/.local/state/mnemo/mnemo.sock
# socket_path = "/Users/you/.local/state/mnemo/mnemo.sock"

# Prefer environment variables for secrets, but config keys are supported.
# elevenlabs_api_key = "..."
# hindsight_api_key = "..."
```

Config precedence is:

```text
built-in defaults < config.toml < environment variables < CLI flags
```

`hindsight_url` is required for `mnemo record`. Set it in the config file,
`MNEMO_HINDSIGHT_API_URL`, or `--hindsight-url`.

`bank` and the ElevenLabs API key are also required for `mnemo record`. Set
`bank` in the selected profile, `MNEMO_BANK_ID`, or `--bank`. Set the
ElevenLabs API key with `MNEMO_ELEVENLABS_API_KEY`, `elevenlabs_api_key` in the
selected profile, or `--elevenlabs-api-key`.

## Profiles

Profiles let you keep separate Hindsight destinations and retain settings in
one config file. Each profile lives under `[profiles.<name>]`.

Example:

```toml
[profiles.default]
hindsight_url = "https://your-hindsight-api.example.com"
bank = "personal"
context = "user recorded voice memo"

[profiles.business]
hindsight_url = "https://your-hindsight-api.example.com"
bank = "business"
context = "user recorded business voice memo"
tags = ["business", "voice-note"]
strategy = "append"

[profiles.family]
hindsight_url = "https://your-hindsight-api.example.com"
bank = "family"
context = "user recorded family voice memo"
metadata = { source = "mnemo", category = "family" }
```

Use the default profile:

```bash
mnemo record
```

Use a named profile:

```bash
mnemo record --profile business
```

Or select a profile with an environment variable:

```bash
export MNEMO_PROFILE="business"
mnemo record
```

Profile values can still be overridden by `MNEMO_*` environment variables or
CLI flags. For example, this records to the `business` profile but overrides
the bank for one run:

```bash
mnemo record --profile business --bank temporary-business-notes
```

Supported environment variables:

```bash
export MNEMO_ELEVENLABS_API_KEY="your-elevenlabs-key"
export MNEMO_HINDSIGHT_API_KEY="your-hindsight-key-if-needed"
export MNEMO_HINDSIGHT_API_URL="https://hindsight-api.example.com"
export MNEMO_PROFILE="default"
export MNEMO_BANK_ID="personal"
export MNEMO_ELEVENLABS_LANGUAGE="eng"
export MNEMO_ELEVENLABS_MODEL="scribe_v2"
export MNEMO_SOCKET_PATH="$HOME/.local/state/mnemo/mnemo.sock"
export MNEMO_CONTEXT="user recorded voice memo"
export MNEMO_TAGS="voice-note,personal"
export MNEMO_STRATEGY="append"
export MNEMO_METADATA='{"source":"mnemo"}'
```

## Development

Check that the project builds:

```bash
cargo check
```

Format the code:

```bash
cargo fmt
```

Run the app in development mode:

```bash
cargo run -- record
```

`mnemo` defaults to `record`, so this is equivalent:

```bash
cargo run
```

Override config from the CLI:

```bash
cargo run -- --bank personal --language auto
```

Use a named profile:

```bash
cargo run -- record --profile business
```

Or with the installed binary:

```bash
mnemo record --profile business
```

Set the Hindsight URL from the CLI:

```bash
cargo run -- --hindsight-url https://your-hindsight-api.example.com
```

Use a different config file:

```bash
cargo run -- --config /path/to/config.toml
```

When running `record`, `mnemo` starts recording immediately. Press `Enter` to
stop recording, then it will transcribe and retain the note.

You can also stop a running recording from another shell:

```bash
cargo run -- stop
```

`record` opens a Unix socket at:

```bash
~/.local/state/mnemo/mnemo.sock
```

Only one `mnemo record` process can run at a time. If the socket belongs to a
live process, a second `record` command exits instead of trying to record from
the microphone at the same time. If the socket is stale, `mnemo` removes it and
continues.

## Build A Release Binary

Build an optimized local macOS binary:

```bash
cargo build --release
```

The binary will be created at:

```bash
target/release/mnemo
```

Run it directly:

```bash
./target/release/mnemo
```

Or explicitly:

```bash
./target/release/mnemo record
```

Stop it from another shell:

```bash
./target/release/mnemo stop
```

Install it somewhere on your `PATH`, for example:

```bash
mkdir -p ~/.local/bin
cp target/release/mnemo ~/.local/bin/mnemo
```

Then run it from anywhere:

```bash
mnemo
```

Make sure `~/.local/bin` is on your `PATH` if the command is not found.

## macOS Notes

The first time you run `mnemo`, macOS may ask for microphone permission for
your terminal app. Approve microphone access, then run the command again if the
first attempt fails.
