#!/bin/sh
# PreToolUse hook: block `cargo clean` in this repo.
#
# ~/.local/bin/frame is a symlink to target/release/frame, which is bound to the
# Print keybindings in ~/.config/mango/config.conf. Removing target/ breaks them
# silently — no error, just a dead key. See CLAUDE.md (Environment) and
# ROADMAP.md, where this correction is recorded rather than quietly deleted.
#
# Wired up in .claude/settings.json under hooks.PreToolUse with matcher "Bash",
# so it runs before every Bash tool call Claude Code makes in this repo.

# Claude Code pipes a JSON object to this script on stdin describing the pending
# tool call. For Bash it looks roughly like:
#   {"tool_name":"Bash","tool_input":{"command":"cargo clean","description":"..."}}
# `cat` reads that whole object; jq pulls out the command string.
#   -r        prints the raw string rather than a JSON-quoted one
#   // empty  yields an empty string instead of the literal "null" if the field
#             is missing, so a malformed payload can't accidentally match below
CMD=$(cat | jq -r '.tool_input.command // empty')

# Substring match, not an exact one — the command could arrive with flags or
# chained after something else (`cd foo && cargo clean`), and all of those are
# equally destructive here.
case "$CMD" in
  *"cargo clean"*)
    # stderr is what Claude Code shows the model as the reason for the block,
    # so this message is written for Claude to read and act on, not just for a
    # human scanning the transcript.
    echo "BLOCKED: cargo clean removes target/release/frame, which ~/.local/bin/frame symlinks to. This silently breaks the Print keybindings. See CLAUDE.md and ROADMAP.md. If you genuinely need it, ask Philipp first — the fix afterwards is a release build plus re-linking." >&2

    # Exit 2 is the only code that actually blocks a PreToolUse call. Exit 1 (or
    # any other non-zero) logs a non-blocking error and the command still runs,
    # which would make this hook decorative.
    exit 2
    ;;
esac

# Exit 0 allows the command through. Every other Bash call in this repo reaches
# here and passes untouched.
exit 0
