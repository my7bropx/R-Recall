# recall — shell integration for fish.
#   recall init fish | source   # add to ~/.config/fish/config.fish
#
# Ctrl-G opens the picker over whatever you've typed so far; Enter fills in
# any placeholders (target, wordlist, ...) right there and drops the finished
# command into your command line, ready to edit or run.

function _recall_pick_widget
    set -l selected (recall pick -- (commandline -b))
    if test -n "$selected"
        commandline -r -- $selected
    end
    commandline -f repaint
end
bind \cg _recall_pick_widget
