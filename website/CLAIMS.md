# Claims register

Every fact a page on muxr.app may state, with where it was verified. Page writers
use this file and nothing else: if a fact is not here, it is not published.

- Fetched and verified: **15 November 2026**. Show that date on any page that
  cites another product ("as published on 15 November 2026").
- This file is never served. The Dockerfile does not copy it into the image.
- To update a claim: re-open its source URL, change the line and its date here,
  then search the site for the old wording and fix every page that used it.
- "Not stated" means the product's own pages say nothing we could find. Write
  "Not stated" on the page; never guess.
- Status words: **verified** (seen on the product's own site, repository or store
  listing, or in this repository's code), **not stated**, **owner to confirm**.
- Three kinds of line appear under each other product:
  1. A sourced claim: `claim — source URL — fetched date — verified`. Publish it
     with a link to that URL.
  2. A "not stated" line. It means: none of the source URLs listed in that
     product's section said anything about this on 15 November 2026. On a page
     write "Not stated" and nothing more.
  3. An "Our judgement" line (where the other product is the better pick, or a
     plain consequence of the facts). It is our conclusion from the sourced
     claims above it, not a quote. Publish it in our own voice, never as
     something the other product says about itself.
- A block marked **DO NOT PUBLISH** is an open question for the owner. Nothing
  inside it may appear on a page until the marker is removed here.
- Pages that still carry the old price wording and must change with it:
  index.html, llms.txt and both repository readmes.

## Muxr

Verified against this repository, the muxr-app repository and the store listings.

### What it is

- Muxr is a mobile app for iPhone, iPad and Android phones and tablets that
  attaches to remote zellij and herdr terminal-multiplexer sessions.
  (website/llms.txt; App Store listing)
- It talks to `muxrd`, a daemon you run on your own machine next to zellij or
  herdr. `muxrctl` is the terminal UI that sets the daemon up and pairs a phone.
  (README.md)
- Publisher: F0X IT LLC. (App Store listing seller line)
- Store listings:
  - App Store: https://apps.apple.com/app/muxr-zellij-herdr-client/id6815185967
  - Google Play: https://play.google.com/store/apps/details?id=com.it.f0x.muxr
- Source: https://github.com/f0x-it-llc/muxr-core (server, this site) and
  https://github.com/f0x-it-llc/muxr-app (the app).

### Platforms

- iPhone and iPad: iOS 15.0 or iPadOS 15.0 and later.
  (App Store listing "Requires iOS 15.0 or later"; muxr-app
  ios/Runner.xcodeproj `IPHONEOS_DEPLOYMENT_TARGET = 15.0`)
- Android phones and tablets: Android 10 and later.
  (muxr-app android/app/build.gradle.kts `minSdk = 29`)
- Desktop apps (macOS, Linux, Windows): coming soon. Do not mention that the
  App Store listing shows Mac or Vision compatibility; the app is not offered
  as a desktop app.
- Server (`muxrd`, `muxrctl`): pre-built for x86_64 Linux, aarch64 Linux and
  Apple Silicon macOS. Intel Macs build from source. Windows is not supported;
  the server needs a Unix host. (README.md § Install)

### What it works with

- zellij: yes. muxrd is built against zellij 0.45.1.
  (muxrd/Cargo.toml `zellij-utils = "0.45.1"`; docker/README.md)
- herdr: yes. muxrd drives a user-installed `herdr` binary over herdr's public
  Unix sockets (the JSON control socket `herdr.sock` and the wire socket
  `herdr-client.sock`). Supported floor 0.9.0; newest tested release 0.9.5.
  (muxrd/src/multiplexer/herdr/mod.rs, paths.rs; docker/README.md)
- herdr-core: not supported. muxrd contains no herdr-core support.
  (muxrd/src/multiplexer/herdr/ has no reference to it)
- Publish this wording: "Muxr works with the herdr 0.9.x server (0.9.0 to
  0.9.5). herdr-core is not supported."
- **DO NOT PUBLISH — owner to confirm:** whether the herdr 0.9.x server muxrd
  drives is the one herdr's site calls "Herdr TUI". Do not use the phrase
  "Herdr TUI" about Muxr, and do not say Muxr works where the Herdr mobile app
  does not.
- muxrd auto-detects whichever of zellij and herdr is installed. (README.md)
- tmux: coming soon. Not available today.
- Without zellij or herdr: Muxr does nothing. It is not a general SSH client.

### How it connects

- The app talks to muxrd over gRPC protected by TLS, signed in with a
  per-device bearer token. (README.md)
- No SSH is used between the app and muxrd.
- Pairing: scan a QR code shown by `muxrctl`. The code carries host, port and
  token; for a self-signed server it also pins the certificate fingerprint, and
  for a CA-signed server the app uses system trust. (index.html "how it works")
- Three certificate modes: self-signed, an external CA certificate, or
  plaintext h2c behind a TLS-terminating proxy. (README.md)
- Default bind: 127.0.0.1:50051. The recommended way to reach the server from
  a phone is a tailnet (Tailscale); a LAN address or a public domain are the
  alternatives. (index.html; docs.html; advanced.html)
- Terminal traffic goes from the phone to your own server. It does not pass
  through any server run by F0X IT LLC. (privacy.html)
- Multiple servers: saved servers and switching between them; more than one
  server needs Muxr Pro. (muxr-app README.md; llms.txt)

### Features shipped

- Sessions: browse, open, create, rename and close sessions, tabs and panes;
  switch spaces on herdr. (index.html)
- Terminal with a key bar (Ctrl, Alt, Esc, arrows and more) and touch gestures:
  tap to focus, tap again for the keyboard, long-press to fullscreen a pane,
  swipe back. (index.html; guide.html)
- Tablet layout: a persistent pane rail beside the terminal. (index.html)
- Agents list: on herdr, Muxr lists the coding agents running in a session,
  most urgent first (blocked, then done, working, idle), with counts.
  (README.md § gRPC contract "Agent inventory")
- On zellij there is no agents list: zellij reports no agents. A coding agent
  running in a zellij pane appears as a terminal like any other pane.
  (README.md: `ListAgents` "answers an empty list on a backend that reports no
  agents")
- Muxr does not integrate with any specific agent. It shows the terminal the
  agent runs in. Which agents herdr reports is up to herdr.
- Read-only tokens: can navigate tabs, panes and spaces, scroll, and resize
  their own terminal; cannot type or change the session. A read-only token can
  also list every herdr space and read any space's tab and pane structure, not
  only the space it is viewing; pane content still requires an attach.
  (README.md § gRPC contract)
- Saved credentials are kept in the iOS Keychain or Android Keystore, gated
  behind device biometrics. (privacy.html; guide.html)
- No account. No analytics, no ads, no crash reporting. (privacy.html)

### Coming soon (not available today; never give a date)

- tmux support
- push notifications. The app does not deliver notifications today. The server
  side already has the plumbing (muxrd can register devices and a relay
  exists, and privacy.html describes an optional "Muxr Push" relay), but
  support.html says device registration arrives "with app-side push support".
  Pages say "coming soon" and do not describe the relay, its platforms or how
  to enable it.
- voice input
- desktop apps for macOS, Linux and Windows

### Price

- Free to download. The free app is the full app with one server.
- Use this sentence verbatim wherever a price appears:

  > Muxr Pro is a one-time $19.99 purchase, 50% off ($9.99) until January 13, 2027.

- Pro unlocks more than one server. No subscription.
- A purchase is made through the App Store or Google Play and belongs to that
  store account. (The App Store listing shows "Muxr Pro $9.99" today.)
- muxrd is free to use under its licence; you host it yourself.

### Licence

- Say **source available**. Never say "open source" about Muxr.
- muxr-core (server, this site): PolyForm Shield License 1.0.0 — free to use,
  modify and share for any purpose, including at work, except to provide a
  product that competes with Muxr. (README.md § License)
- muxr-app (the app): PolyForm Shield License 1.0.0, the same licence as the
  server — free to use, modify and share for any purpose, including at work,
  except to provide a product that competes with Muxr. (Owner decision,
  15 November 2026; muxr-app LICENSE.)
- Shield's one limit is on providing a competing product, which covers
  reselling the app. It is not a limit on commercial or work use.
- Release note, not for pages: the muxr-app LICENSE changes in the same
  release as this site. Do not deploy the site before the muxr-app pull
  request that changes the licence has merged.

### Demo server

- A public read-only demo runs at demo.muxr.app. (advanced.html)
- Publish at most: "A public read-only demo server runs at demo.muxr.app."
- **DO NOT PUBLISH — owner to confirm:** how a visitor gets a token for it.
  The site publishes none. Do not tell readers they can try Muxr without
  installing a server.

### Support

- Email: support@muxr.app (support.html). Use this for vulnerability reports
  too; no separate security address is published.

## Disambiguation

Use this sentence verbatim:

> Muxr by F0X IT LLC (muxr.app) is a zellij and herdr client and is unrelated to the npm package @trymuxr/cli at trymuxr.com.

About that other product, for reference only (do not describe it on our pages
beyond the sentence above):

- A self-hosted remote control for coding agents, installed with npm
  (`@trymuxr/cli`), also named "muxr". — https://trymuxr.com — 15 Nov 2026 — verified
- npm package created 16 August 2026, Apache-2.0. —
  https://registry.npmjs.org/@trymuxr/cli — 15 Nov 2026 — verified
- Make no claim about which product came first.

## Other products

Format: claim — source — fetched — status.

### Moshi

- "A terminal built for phones"; a mobile terminal that connects by SSH, Mosh
  or Eternal Terminal to your own machine to supervise coding agents. —
  https://getmoshi.app/ and https://getmoshi.app/llms.txt — 15 Nov 2026 — verified
- Platforms: iPhone, iPad, Apple Watch, Apple Vision (iOS 18.0 or later);
  Android; a desktop app. — https://apps.apple.com/us/app/id6757859949 and
  https://getmoshi.app/ — 15 Nov 2026 — verified
- Free tier: "a complete terminal — SSH, agent monitoring, and push
  notifications, free forever. No account, no credit card". —
  https://getmoshi.app/pricing — 15 Nov 2026 — verified
- Prices on its website: Pro $9.99 / month, $89.99 / year, Lifetime $199 once;
  every plan covers up to 3 devices. — https://getmoshi.app/pricing —
  15 Nov 2026 — verified
- Prices on its App Store listing differ: Pro Monthly listed at $6.99, $7.99
  and $9.99; Pro Yearly at $69.99 and $89.99; Pro Lifetime $249.00. —
  https://apps.apple.com/us/app/id6757859949 — 15 Nov 2026 — verified
- **Rule:** a page that gives a Moshi price shows both sources ("$199 on its
  website, $249.00 on the App Store"), never a single figure.
- Paying unlocks Mosh and Eternal Terminal sessions and auto-attach for tmux
  and herdr. — https://getmoshi.app/pricing — 15 Nov 2026 — verified
- zellij: supported, with a session picker and shortcut panel. —
  https://getmoshi.app/docs/zellij — 15 Nov 2026 — verified
- herdr: supported, with a workspace picker and shortcut panel. —
  https://getmoshi.app/guides/herdr — 15 Nov 2026 — verified
- tmux: supported. — https://getmoshi.app/guides/tmux — 15 Nov 2026 — verified
- Connection: SSH, Mosh or ET straight to your host; no relay; no server
  install required; an optional hook adds agent context. —
  https://getmoshi.app/ — 15 Nov 2026 — verified
- Agent features: push notifications when agents need you, approvals from the
  Lock Screen and Apple Watch, an agents board, on-device voice dictation, a
  diff viewer. — https://getmoshi.app/ — 15 Nov 2026 — verified
- Account required: no. — https://getmoshi.app/pricing — 15 Nov 2026 — verified
- Source code and licence: not stated.
- Our judgement — better pick than Muxr when: you need tmux today, push notifications, voice
  input, Apple Watch, or you do not want to install anything on the server.

### Zelmux

- "A mobile client for Zellij, Tmux and SSH", "optimized for Claude and Codex
  cli". — https://apps.apple.com/us/app/zelmux/id6768126927 — 15 Nov 2026 — verified
- Free. iPhone and iPad, iOS 16.0 or later. Version 1.3. — same — verified
- Attaches to zellij sessions over HTTPS and WebSocket; supports self-signed
  TLS with certificate pinning. — same — verified
- Android: not stated (the listing is App Store only). herdr: not stated.
  Push notifications: not stated. Source code: not stated.
- Our judgement — better pick than Muxr when: you want a free zellij client on iPhone or iPad,
  or one app for zellij, tmux and plain SSH.

### Termius

- "Modern SSH client"; platforms macOS, Windows, Linux, iOS, Android. —
  https://termius.com/ — 15 Nov 2026 — verified
- Prices: Starter free; Pro US$10 per month; Team US$20 per seat per month;
  Business US$30 per seat per month; Enterprise custom. —
  https://termius.com/pricing — 15 Nov 2026 — verified
- Protocols: SSH, SFTP, Telnet, Mosh. — https://termius.com/pricing —
  15 Nov 2026 — verified
- zellij, herdr or tmux-specific features: not stated.
- Our judgement — it is a general SSH client, so any multiplexer runs inside
  the shell session.
- One-time purchase: not stated (plans are per month). Source code: not stated.
- Our judgement — better pick than Muxr when: you manage many servers over SSH, need desktop
  apps and sync, SFTP or port forwarding, or use tmux.

### Blink Shell

- "A professional, desktop grade terminal for iOS." iPhone and iPad only. —
  https://blink.sh/ — 15 Nov 2026 — verified
- Price: "Get 2 weeks free. Then just $19.99/year." — https://blink.sh/ —
  15 Nov 2026 — verified
- Open source, GPL-3.0; SSH and Mosh. — https://github.com/blinksh/blink —
  15 Nov 2026 — verified
- Our judgement — no Android app: its own site describes it as a terminal for iOS.
- zellij or herdr-specific features: not stated.
- Our judgement — better pick than Muxr when: you want a full SSH and Mosh terminal on iPad
  with a hardware keyboard, open-source code, or tmux.

### Prompt 3

- By Panic. iPhone, iPad, Mac, Apple Vision. Free with in-app purchases:
  $9.99 per year, or $49.99 one time. SSH, Mosh, Eternal Terminal. —
  https://apps.apple.com/app/id1594420480 — 15 Nov 2026 — verified
- Our judgement — no Android app: its store listing covers Apple platforms only.
- zellij or herdr-specific features: not stated.

### Secure ShellFish

- "Lightning fast SSH terminal with remote file access." iPhone, iPad, Mac,
  Apple Vision, Apple Watch. Free with in-app purchases: $2.99 per month,
  $14.99 per year, or a $29.99 lifetime unlock. Built-in tmux support. —
  https://apps.apple.com/us/app/ssh-client-secure-shellfish/id1336634154 —
  15 Nov 2026 — verified
- Detects Claude Code, Codex CLI and OpenCode and sets up agent hooks for
  notifications. — same — verified
- Our judgement — no Android app: its store listing covers Apple platforms only.
- zellij or herdr-specific features: not stated.

### Termux

- "An Android terminal emulator and Linux environment app"; free; distributed
  through F-Droid and GitHub. — https://termux.dev/en/ — 15 Nov 2026 — verified
- Our judgement — use with zellij: run `ssh` inside Termux and then `zellij attach`.

### ConnectBot

- An SSH client for Android, Apache-2.0. —
  https://github.com/connectbot/connectbot — 15 Nov 2026 — verified

### zellij web client

- Part of zellij. Lets you attach to existing sessions, or start new ones, from
  a browser. Off by default; started with `zellij web`. —
  https://zellij.dev/documentation/web-client — 15 Nov 2026 — verified
- HTTPS is required when it listens on anything other than 127.0.0.1. Login
  tokens are required; read-only tokens exist. — same — verified
- On phones it detects mobile browsers and offers touch scrolling, on-screen
  keys, pane and tab pickers and single-pane fullscreen; it can be installed as
  a web app. — same — verified
- No built-in rate limiting; a reverse proxy is recommended. — same — verified
- Our judgement — free, because it ships with zellij; it is a zellij feature, so it does not attach to herdr.
- Push notifications: not stated.
- Our judgement — better pick than Muxr when: you want zellij on a phone with no app and no
  extra daemon, or only need occasional access.

### An SSH app running `zellij attach`

Our judgement throughout: this section describes a method, not a product, and
quotes nobody. Name the SSH apps only as examples, with the facts from their
own sections.

- Works with any SSH client (Termius, Blink Shell, Prompt 3, Termux,
  ConnectBot). Needs only an SSH server on the machine.
- The session is the ordinary zellij terminal UI, drawn at the phone's screen
  size and driven by zellij's keyboard shortcuts.
- Our judgement — better pick than Muxr when: you already live in an SSH app, or cannot install
  a daemon on the server.

### Herdr mobile app (made by the herdr project)

- "Herdr for iPhone, iPad and Android": see which agents need you, read what
  they did and answer them. — https://herdr.dev/mobile — 15 Nov 2026 — verified
- Free. — https://herdr.dev/mobile ("Is it free? Yes.") — 15 Nov 2026 — verified
- Connects over SSH to herdr-core on each machine; "There's no Herdr account
  and no server of ours in between." — same — verified
- "Works with herdr-core, which comes with the Herdr app. Herdr TUI isn't
  supported." — same — verified
- Inbox sorted by needs you, done, working, idle. — same — verified
- Away from home: Tailscale or any VPN. — same — verified
- herdr itself is open source under Apache 2.0. — https://herdr.dev/ —
  15 Nov 2026 — verified
- Push notifications: not stated.
- Our judgement — it is a herdr client; its page mentions no other multiplexer.
- Our judgement — better pick than Muxr when: you run herdr-core. It is free and made by the
  herdr project.

### Heeler for herdr

- A native iPhone and iPad app for herdr; free; connects over SSH. —
  https://apps.apple.com/us/app/heeler-for-herdr/id6797263135 — 15 Nov 2026 — verified
- Push notifications: not stated. Licence: not stated.

### Collie

- "A self-hosted PWA to drive Claude Code, Pi, Codex and OpenCode in Herdr,
  tmux or zellij from your phone." MIT. Runs a process on the computer and is
  opened in the phone's browser; push when an agent pauses or finishes. —
  https://github.com/AltanS/collie — 15 Nov 2026 — verified

### Sherpr

- An Android app for herdr over SSH, with no service in between; GPL-3.0;
  on F-Droid; Android 7.0 or later; on-device notifications. —
  https://f-droid.org/packages/org.sherpr.app/ — 15 Nov 2026 — verified

### herdrup

- An iPhone and iPad app to answer coding agents; Apache-2.0; free; needs
  herdrup's own fork of herdr; push through its relay. —
  https://github.com/themartianapp/herdrup — 15 Nov 2026 — verified

### Whip

- "An agent-native SSH mobile client, designed specifically for the Herdr
  multiplexer." Android and iOS; AGPL-3.0; opt-in push through a relay. —
  https://github.com/kosumic/whip — 15 Nov 2026 — verified
- Price: not stated.

### herdr-mobile-relay

- "Approve and monitor Herdr agents remotely from your phone." An installable
  mobile web app with a relay on each computer; AGPL-3.0. —
  https://github.com/0cv/herdr-mobile-relay — 15 Nov 2026 — verified

### Claude Code Remote Control (Anthropic)

- Continues a local Claude Code session from claude.ai/code or the Claude iOS
  and Android apps. The session runs on your computer. —
  https://code.claude.com/docs/en/remote-control — 15 Nov 2026 — verified
- The local session makes outbound HTTPS requests only; traffic goes through
  the Anthropic API, and the transcript is stored on Anthropic's servers while
  connected. — same — verified
- Needs a paid Claude plan; API keys are not supported. — same — verified
- The computer must stay on with `claude` running. — same — verified
- The phone shows the conversation; a terminal emulator is not described. —
  same — verified
- Works with Claude Code only. — same — verified
- Our judgement — better pick than Muxr when: you use Claude Code only, do not need the
  terminal, and want Anthropic's own notifications with nothing to self-host.

### Codex remote connections (OpenAI)

- The ChatGPT app on iOS or Android connects to a Codex host (the ChatGPT
  desktop app on macOS or Windows) through "a secure relay layer"; from the
  phone you can start and continue threads, approve actions and review diffs
  and terminal output. — https://learn.chatgpt.com/docs/remote-connections —
  15 Nov 2026 — verified
- An interactive terminal: not stated. Plans required: not stated.

### Happy

- "Free and open source" (MIT) mobile and desktop client for Claude Code,
  Codex, Grok and Gemini; iOS and Android; QR pairing; end-to-end encrypted. —
  https://github.com/slopus/happy — 15 Nov 2026 — verified
- Terminal or conversation view: not stated.

### Tactic Remote

- "Remote control AI coding agents from anywhere." iPhone and iPad, with a
  companion for macOS, Windows and Linux; direct or relay connection; Claude
  Code, Codex, Amp, Droid; a chat view and a native terminal. —
  https://tacticremote.com/ — 15 Nov 2026 — verified
- Price: not stated. Licence: not stated. Android: not stated.

### Cosyra

- "A mobile cloud terminal for AI coding agents": a hosted cloud machine, not
  your own computer; iPhone and Android; $30 per month for its 8 GB plan. —
  https://cosyra.com/ — 15 Nov 2026 — verified

### KittyLitter

- "A native ios + android client for codex, claude, opencode, pi, and droid";
  "open source and always free"; connects over the local network, SSH or QR
  pairing. — https://kittylitter.app/ — 15 Nov 2026 — verified
- Terminal or conversation view: not stated.

## Products we do not cite

- JuiceSSH: only third-party pages describe its status. Leave it out.
- Any price or feature seen only on another vendor's comparison page.
