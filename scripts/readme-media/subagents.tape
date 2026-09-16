Output "out/subagents.gif"
Source theme.tape
Hide
Type `asciinema rec -q --overwrite -i 3 -c "kit tui --root $KIT_DEMO_ROOT" out/subagents.cast`
Enter
Sleep 3s
Show
Sleep 500ms
Type "Spawn two subagents in parallel: one on harness acp.claude that reviews calc.py for edge cases, one on acp.codex that proposes one extra test. Then prompt the first subagent again to rank its findings. Summarize both."
Enter
Sleep 180s
Hide
Ctrl+C
Sleep 1s
