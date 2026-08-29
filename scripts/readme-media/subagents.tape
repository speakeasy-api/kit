Output "out/subagents.gif"
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
Type "Spawn two subagents in parallel: one on harness acp.claude that reviews calc.py for edge cases, one on acp.codex that proposes one extra test. Then prompt the first subagent again to rank its findings. Summarize both."
Enter
Sleep 240s
Hide
Ctrl+C
Sleep 1s
