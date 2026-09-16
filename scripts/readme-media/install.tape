Output "out/install.gif"
Source theme.tape
Set Height 1000
Hide
Type `PROMPT_EOL_MARK='' PROMPT='%F{blue}> %f' asciinema rec -q --overwrite -i 3 -c 'zsh -f' out/install.cast`
Enter
Sleep 1s
Type "export KIT_INSTALL_DIR=$(mktemp -d) && export PATH=$KIT_INSTALL_DIR:$PATH && { command -v docker >/dev/null || alias docker=podman; } && clear"
Enter
Sleep 500ms
Show
Type "curl -fsSL https://raw.githubusercontent.com/speakeasy-api/kit/main/install.sh | sh"
Enter
Sleep 8s
Type "kit --version"
Enter
Sleep 2s
Type "docker run --rm ghcr.io/speakeasy-api/kit:latest --version"
Enter
Sleep 15s
Hide
Type "exit"
Enter
Sleep 1s
