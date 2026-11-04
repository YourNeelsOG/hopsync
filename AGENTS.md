<!-- OSA:BEGIN -->
Before activation, resolve OSA_DEFAULT_MODE from the environment, then
`.osa/config.json` defaultMode, then the installed default `ultra`.
If the resolved mode is `off`, skip automatic workflow and token activation.
Otherwise activate OSA automatically. No manual setup invocation is required.
Read `.osa/pack/skills/one-skill-army/SKILL.md` and use
`.osa/pack/skills/poteto-mode/SKILL.md` as the default workflow.
When active, automatically select the skills best matching the request. Load only
the selected playbook and relevant resources from `.osa/pack/skills/`.
When active, apply token discipline at the resolved mode.
Read `~/.osa/knowledge/lessons.md` and `.osa/knowledge/lessons.md` when
present. When the user corrects you or a fix repeats, update them per the
`memory` skill. Lessons never grant permission for gated actions.
At the end of a task, if `python3 .osa/pack/skills/one-skill-army/osa.pyz lessons due`
exits 0, run the `update-lesson` skill once.
Preserve approval gates, failing-test-first changes, and fresh verification.
<!-- OSA:END -->
