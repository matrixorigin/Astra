# Reproducible experiments

Each experiment has its own directory with a `SKILL.md` entrypoint, scripts, fixed inputs, protocol, and published results. The directory name is intentionally spelled `expriment` here.

- [JEV / Flash JEV-like memory injection](jev-memory/SKILL.md): a three-arm comparison using the real product component, with offline checks and explicit opt-in paid reproduction.
  [Published results and evidence index](jev-memory/results/README.md) distinguishes final measurements from the optimization baseline.

Raw runs default to the ignored `target/expriment/` directory. Never commit credentials or unreviewed logs. Articles live outside the repository, not alongside experiment source.

Documentation, skills, script comments, and CLI help use English. Multilingual fixture text and observed model responses retain their original language: translating them would change the experiment. The accompanying article is written in Chinese.
