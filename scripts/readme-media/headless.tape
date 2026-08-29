Output "out/headless.gif"
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 560
Set Theme "Catppuccin Mocha"
Set Padding 12
Set TypingSpeed 40ms
Hide
Type "cd $KIT_DEMO_ROOT && export GIT_PAGER=cat PAGER=cat && clear"
Enter
Show
Type "kit prompt 'Does calc.py guard against empty input? One sentence.'"
Enter
Sleep 25s
Type "kit sessions | cut -f2,3 | head -3"
Enter
Sleep 3s
Type "kit prompt --resume $(kit sessions | sed -n 2p | cut -f2) 'Add the guard, run the tests, one line summary.'"
Enter
Sleep 45s
Type "git --no-pager diff --stat"
Enter
Sleep 4s
