# Slack DocsGPT extension

Slack bots for your [DocsGPT](https://www.docsgpt.cloud/) agents. One small binary runs any number of Slack apps, each connected to one or more agents. It uses Slack's agent experience:
- answers stream into the conversation as they are written, with a native Stop button
- sources and 👍/👎 buttons sit under each answer
- files you send go to the agent, and files the agent's tools produce come back

Version 2 is a rewrite in Rust on [`docsgpt-rs`](https://github.com/arc53/docsgpt-rs). The Python bot lives on the [`legacy-python`](https://github.com/arc53/slack-bot-docsgpt-extenstion/tree/legacy-python) branch. Existing `.env` files keep working (see [Upgrading from version 1](#upgrading-from-version-1)).

## Features

- **Streaming answers.** Answers stream into DMs and channel threads (`chat.startStream`). Tool steps show as tasks ("Running code"). The native Stop button ends the answer.
- **Agent sessions.** Each DM message starts its own session with a title. Suggested prompts appear when someone opens the bot's Messages tab.
- **Channels.** Mention the bot to start a thread; it follows up on replies in that thread without another mention. This is configurable: `thread`, `mention` or `off`.
- **Sources and feedback.** Sources are linked under the answer. 👍/👎 are sent to DocsGPT as feedback on that exact answer.
- **Files in.** Documents and images you attach are sent to the agent. Voice clips with no text are transcribed and asked as the question.
- **Files out.** When the agent runs tools (code execution, document or image generation), the files are uploaded to the thread.
- **Many apps, many agents.** A TOML file declares bots and their agents. `#sales question` asks one agent once; `/docsgpt agent sales` switches the conversation's agent.
- **Socket Mode or HTTP.** Socket Mode needs no public URL. The HTTP Events API is for deployments behind a load balancer, and verifies Slack's request signature.
- **Operations.** SQLite storage by default, structured logs, `/healthz`, graceful shutdown that lets answers finish, and a small distroless image for amd64 and arm64.

## Quick start

You need a Slack workspace where you can install apps, and an agent API key from DocsGPT (Agents → your agent → API key).

> Some of Slack's AI features (streaming, agent sessions) need a paid Slack plan. A free [Developer Program sandbox](https://api.slack.com/developer-program) works for trying it out. Without them the bot posts each answer as one message.

### 1. Create the Slack app

The bot prints its own app manifest:

```bash
docker run --rm -e SLACK_BOT_TOKEN=xoxb-x -e SLACK_APP_TOKEN=xapp-x -e API_KEY=x \
  arc53/slack-bot-docsgpt-extenstion:latest --print-manifest --app-name "DocsGPT"
```

1. Go to [api.slack.com/apps](https://api.slack.com/apps) → **Create New App** → **From a manifest**. Pick the workspace and paste the JSON.
2. **Install to Workspace**.
3. Copy the **Bot User OAuth Token** (`xoxb-…`) from *OAuth & Permissions*.
4. For Socket Mode, go to *Basic Information* → **App-Level Tokens** → generate one with `connections:write` (`xapp-…`).
5. For HTTP mode, copy the **Signing Secret** from *Basic Information* instead. Set `PUBLIC_URL` before printing the manifest, so the request URLs (`<PUBLIC_URL>/slack/docsgpt/events`) are filled in.

The manifest turns on Slack's agent experience (`agent_view`). Slack doesn't let you switch that back off for the app.

### 2. Run it

```bash
git clone https://github.com/arc53/slack-bot-docsgpt-extenstion.git
cd slack-bot-docsgpt-extenstion
cp .env.example .env      # fill in SLACK_BOT_TOKEN, SLACK_APP_TOKEN and API_KEY
docker compose up -d
```

Or without compose:

```bash
docker run -d --name docsgpt-slack --env-file .env -v botdata:/app/data arc53/slack-bot-docsgpt-extenstion:latest
```

`docsgpt-slack --check` validates the config and the Slack tokens, then exits.

### From source

```bash
cargo build --release
./target/release/docsgpt-slack --check
./target/release/docsgpt-slack
```

Rust 1.88 or newer is required.

## Configuration

### One bot: environment variables

| Variable | Purpose |
|---|---|
| `SLACK_BOT_TOKEN` | Bot token (`xoxb-…`). Required. |
| `SLACK_APP_TOKEN` | App-level token (`xapp-…`) for Socket Mode. |
| `SLACK_SIGNING_SECRET` | Use HTTP mode instead of Socket Mode. Slack posts to `<PUBLIC_URL>/slack/docsgpt/events`. |
| `PUBLIC_URL`, `HTTP_BIND` | HTTP mode: the public URL (for the manifest) and the listen address (default `0.0.0.0:8080`). |
| `API_KEY` | DocsGPT agent API key for the default agent. |
| `API_KEY_<NAME>` | Extra agents, addressed as `#name`. |
| `API_BASE` | DocsGPT server URL (default `https://gptcloud.arc53.com`). |
| `SQLITE_PATH` / `STORAGE_TYPE` | SQLite file (default `data/docsgpt-slack.db`; `/app/data/…` in Docker), or `STORAGE_TYPE=memory`. |
| `CHANNELS` | `thread` (default), `mention` or `off`. |
| `STREAMING`, `AGENT_VIEW`, `REACTIONS`, `FILES` | `false` turns that feature off. |
| `MAX_FILE_MB`, `ALLOWED_CHANNELS` | File size limit (default 20); comma-separated channel ids to answer in (DMs are always allowed). |
| `AGENT_DESCRIPTION` | Shown in Slack's agent directory (via the manifest). |
| `RUST_LOG`, `LOG_FORMAT=json` | Logging. |

### Many bots: `docsgpt-slack.toml`

Create `docsgpt-slack.toml` in the working directory, or set `DOCSGPT_SLACK_CONFIG=/path/to/file`.
- `${VAR}` and `${VAR:-default}` are filled in from the environment, so secrets can stay in `.env`.
- [`docsgpt-slack.example.toml`](docsgpt-slack.example.toml) shows every option.
- In Docker, mount the file: `-v ./docsgpt-slack.toml:/app/docsgpt-slack.toml:ro`.
- `--print-manifest --bot <name>` prints the manifest for one bot.

Per-bot options:
- `mode` (`socket`/`http`), `channels`, `streaming`, `agent_view`, `reactions`
- `files`, `max_file_mb`, `allowed_channels`
- `description`, `suggested_prompts`, `command`, `api_base`

All HTTP-mode bots share one server on `server.bind`. Each bot has its own path, `/slack/<bot name>/events`, which takes events, interactivity and slash commands. `/healthz` answers `ok`.

### Storage

DocsGPT keeps the conversation transcript. The bot only stores:
- which DocsGPT conversation each Slack thread is in
- each channel's chosen agent
- which message holds which answer, so 👍/👎 can be sent later

Backends:
- `sqlite` (default): one file, kept in the `/app/data` volume in Docker.
- `memory`: lost on restart.

## Using the bot

- **In a DM**, ask anything. Each message starts its own session; reply in its thread to continue the conversation. Press **Stop** to end an answer early.
- **In a channel**, mention `@DocsGPT` to start a thread. Later replies in that thread are answered without a mention, unless a reply mentions someone else or starts with `!`.
- **Several agents:**
  - `#sales what does it cost?` asks one agent once.
  - `/docsgpt agent sales` switches agents for the DM or channel.
  - A message that is just `#sales` does the same.
  - `/docsgpt agents` lists them.
- **Files:** attach them to your message. A voice clip without text is transcribed first.
- **Feedback:** 👍/👎 under an answer is recorded in DocsGPT for that answer.

## Limits

Slack limits its streaming methods per workspace, not per message:
- `chat.appendStream` allows about 100 calls a minute across the whole workspace.
- `chat.startStream` and `chat.stopStream` allow about 20 calls a minute.

The bot updates each streaming answer at most every 1.2 s and keeps within these budgets. When too many answers stream at once, the extra ones are posted as a single message when they're done.

## Upgrading from version 1

- **Branches and images:** the `legacy-python` branch keeps the Python bot (build it from there; version 1 had no published image). `:latest` and `:2` are the Rust bot.
- **`.env`:** works as is. `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`, `API_KEY` and `API_KEY_<NAME>` mean the same.
- **MongoDB is no longer supported.** Remove `STORAGE_TYPE=mongodb` and the `MONGODB_*` variables, and mount a volume for `/app/data`. Version 1 kept its own copy of each conversation; version 2 lets DocsGPT keep it, so old threads start a fresh conversation.
- **Recreate the app from the new manifest** (`--print-manifest`), or add the scopes and events it lists to your existing app and reinstall it. Version 2 needs streaming, agent sessions, files and the slash command.
- **Default changes:** answers now stream, and in channels the bot follows its own threads. Set `CHANNELS=mention` for the old mention-only behaviour.

## Development

```bash
cargo test            # unit tests + end-to-end tests against a mock Slack API and a mock DocsGPT
cargo clippy --all-targets -- -D warnings
```

The end-to-end tests (`tests/`) run the real bot against an in-process mock of the Slack Web API, which records calls and serves a Socket Mode WebSocket, and the `docsgpt` crate's mock server.

Layout:
- `src/config.rs`: TOML and environment config.
- `src/slack/`: Web API client, Socket Mode, HTTP mode, signature checks.
- `src/events.rs`: what each event does.
- `src/surface.rs`: how an answer is streamed and finished.
- `src/render.rs`: blocks and text.
- `src/files.rs`: files.
- `src/manifest.rs`: the app manifest.

## License

MIT — see [LICENSE](LICENSE).
