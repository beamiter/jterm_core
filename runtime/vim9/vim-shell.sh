#!/bin/sh
# Invoked by the user's shell after expansion, quoting, aliases and redirection.
# It never evaluates a command string or changes the user's rc files.

real_vim=
remaining_path=${PATH-}
while :; do
    case "$remaining_path" in
        *:*) directory=${remaining_path%%:*}; remaining_path=${remaining_path#*:}; last=0 ;;
        *) directory=$remaining_path; last=1 ;;
    esac
    [ -n "$directory" ] || directory=.
    candidate=$directory/vim
    if [ -f "$candidate" ] && [ -x "$candidate" ] && ! [ "$candidate" -ef "$0" ]; then
        case "$candidate" in
            /*) real_vim=$candidate ;;
            *) real_vim=$PWD/$candidate ;;
        esac
        break
    fi
    [ "$last" = 0 ] || break
done
if [ -z "$real_vim" ]; then
    printf '%s\n' 'vim: no original Vim executable found in PATH' >&2
    exit 127
fi

fallback=0
case "${JTERM_VIM9_AUTO_GUI-1}" in 0|false|off) fallback=1 ;; esac
[ -t 0 ] && [ -t 1 ] || fallback=1
[ -n "${DISPLAY-}${WAYLAND_DISPLAY-}" ] || fallback=1
[ -n "${JTERM_VIM9_FRONTEND-}" ] && [ -x "${JTERM_VIM9_FRONTEND-}" ] || fallback=1

# Ex/silent/script stdin, client-server commands and queries keep normal Vim
# behavior. Do not mistake an option's operand or a filename after -- for a flag.
operand=0
for argument do
    if [ "$operand" = 1 ]; then operand=0; continue; fi
    if [ "$operand" = 2 ]; then
        operand=0
        # -S defaults to Session.vim when followed by another option.
        case "$argument" in -*) ;; *) continue ;; esac
    fi
    case "$argument" in
        --) break ;;
        -|-h|--help|--version|--gui|--not-a-term|--remote*|--server*|--socketid|--echo-wid) fallback=1 ;;
        -S) operand=2 ;;
        -u|-U|-i|-T|-t|-q|-c|-w|-W|--cmd|--startuptime) operand=1 ;;
        -u*|-U*|-i*|-T*|-t*|-q*|-c*|-S*|-w*|-W*|-V*) ;;
        --*) ;;
        -*) case "$argument" in *[eEsgy]*) fallback=1 ;; esac ;;
    esac
done

if [ "$fallback" = 1 ]; then
    exec "$real_vim" "$@"
fi
exec "$JTERM_VIM9_FRONTEND" --vim-shell "$real_vim" -- "$@"
