Output "out/headless.gif"
Source theme.tape
Set Height 800
Hide
Type `PROMPT_EOL_MARK='' PROMPT='%F{blue}> %f' asciinema rec -q --overwrite -i 3 -c 'zsh -f' out/headless.cast`
Enter
Sleep 1s
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
Hide
Type "exit"
Enter
Sleep 1s
