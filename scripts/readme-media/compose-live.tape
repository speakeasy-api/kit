Output "out/compose-live.gif"
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 720
Set Theme "Catppuccin Mocha"
Set Padding 12
Set TypingSpeed 40ms
Hide
Type "kit tui --root $KIT_DEMO_ROOT"
Enter
Sleep 3s
Show
Sleep 500ms
Type "In ONE compose program: run `python3 -m unittest` 3 times in sequence with a 3 second sleep between runs, and concurrently count lines in every .py file. Return a summary object."
Enter
Sleep 70s
Hide
Ctrl+C
Sleep 1s
