# Muxr demo login profile — friendly, harmless, read-only sandbox.
# (Sourced for the reviewer's interactive shell inside the demo container.)

# A clear prompt so it reads as a real terminal.
export PS1='\[\e[38;5;39m\]muxr-demo\[\e[0m\]:\[\e[38;5;245m\]\w\[\e[0m\]$ '
export EDITOR=nvim
export PAGER=less

# Handy read-only aliases.
alias ll='ls -alh --color=auto'
alias ls='ls --color=auto'
alias tree='tree -C'
# nvim opens read-only by default here; the muxr-core clone is chmod a-w
# anyway, so :w fails regardless.
alias vim='nvim -R'
alias vi='nvim -R'

# One-time welcome.
if [ -z "${MUXR_DEMO_WELCOMED:-}" ]; then
  export MUXR_DEMO_WELCOMED=1
  cat <<'WELCOME'

  Welcome to the Muxr demo terminal.
  This is a real, sandboxed shell attached to a real read-only clone of the
  open-source muxr-core repo: no sudo, no network egress, read-only files,
  and everything resets when the demo restarts. Explore freely:

    git log --oneline    bat muxrd/src/lib.rs    rg AttachTerminal    tree

  `opencode` is installed but cannot reach a model here — the demo has no
  network egress and ships no API key, so its login screen is expected
  sandboxing, not a broken product.

WELCOME
fi
