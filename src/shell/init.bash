# recall — shell integration for bash.
#   eval "$(recall init bash)"   # add to ~/.bashrc
#
# Ctrl-G opens the picker over whatever you've typed so far; Enter fills in
# any placeholders (target, wordlist, ...) right there and drops the finished
# command into your command line, ready to edit or run.

_recall_pick_widget() {
  local selected
  selected=$(recall pick -- "$READLINE_LINE")
  if [[ -n "$selected" ]]; then
    READLINE_LINE="$selected"
    READLINE_POINT=${#READLINE_LINE}
  fi
}
bind -x '"\C-g": _recall_pick_widget'
