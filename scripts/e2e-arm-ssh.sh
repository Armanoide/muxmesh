#!/usr/bin/env bash
# e2e-arm-ssh.sh — make a fake "ssh mac_mini" process appear in a tmux pane
# so armed-host detection (pane_ssh_host -> looks_like_ssh + ssh_target_of)
# arms the window toward mac_mini, exactly as a real `ssh mac_mini` would.
# looks_like_ssh checks argv[0] basename == "ssh"; ssh_target_of takes the
# first non-flag token. exec -a sets argv[0]="ssh" and we pass the target as
# a real argument so ps shows "ssh mac_mini". sleep stays alive.
TARGET="${1:-mac_mini}"
exec -a ssh /usr/bin/sleep "$TARGET" 3600
