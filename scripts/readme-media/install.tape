Output "out/install.gif"
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 500
Set Theme "Catppuccin Mocha"
Set Padding 12
Set TypingSpeed 40ms
Type "curl -fsSL https://raw.githubusercontent.com/speakeasy-api/kit/main/install.sh | sh"
Enter
Sleep 8s
Type "kit --version"
Enter
Sleep 2s
Type "docker run --rm ghcr.io/speakeasy-api/kit:latest --version"
Enter
Sleep 15s
