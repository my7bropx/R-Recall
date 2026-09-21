# recall — shell integration for zsh.
#   eval "$(recall init zsh)"   # add to ~/.zshrc
#
# Ctrl-G opens the picker over whatever you've typed so far; Enter fills in
# any placeholders (target, wordlist, ...) right there and drops the finished
# command into your command line, ready to edit or run.

_recall_pick_widget() {
  local selected
  selected=$(recall pick -- "$LBUFFER")
  if [[ -n "$selected" ]]; then
    LBUFFER="$selected"
  fi
  zle reset-prompt
}
zle -N _recall_pick_widget
bindkey '^G' _recall_pick_widget
