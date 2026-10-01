"""Point et-mp's live thornlist at the mixed-precision forks, as notebook 3
does (its cell makes the same three edits; the build replay only matches
if the resulting thornlist does)."""

from pathlib import Path

live = Path.home() / "et-mp/thornlists/installation-default.th"
text = live.read_text()
edits = [
    ("!URL      = https://bitbucket.org/cactuscode/cactus.git\n!REPO_BRANCH = $ET_RELEASE",
     "!URL      = https://github.com/max-morris/Cactus.git\n!REPO_BRANCH = mixed-precision"),
    ("!URL      = https://github.com/EinsteinToolkit/CarpetX\n!REPO_BRANCH = $ET_RELEASE",
     "!URL      = https://github.com/max-morris/CarpetX.git\n!REPO_BRANCH = mixed-precision"),
    ("CarpetX/TestProlongate\n", "CarpetX/TestProlongate\nCarpetX/TestReal4\n"),
]
for old, new in edits:
    if new not in text:
        assert text.count(old) == 1, old
        text = text.replace(old, new)
live.write_text(text)
