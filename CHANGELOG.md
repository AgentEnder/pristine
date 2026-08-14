## 0.1.0 (2026-08-14)

### 🚀 Features

- scaffold the cargo workspace, mise toolchain, Nx layer and CI ([9bb4a44](https://github.com/AgentEnder/pristine/commit/9bb4a44))
- the parallel walker and the tier-one marker ruleset ([#587](https://github.com/AgentEnder/pristine/pull/587))
- the tier-two gitignore fallback ([#588](https://github.com/AgentEnder/pristine/pull/588))
- the --min-size flag, and a survey that admits a short listing ([#588](https://github.com/AgentEnder/pristine/pull/588))
- the deleter and the safety model ([#594](https://github.com/AgentEnder/pristine/pull/594))
- repo mode, git-aware cleaning of one checkout ([#595](https://github.com/AgentEnder/pristine/pull/595))
- the npm wrapper and one binary package per platform ([#596](https://github.com/AgentEnder/pristine/pull/596))
- cut every distribution channel from one tag ([#597](https://github.com/AgentEnder/pristine/pull/597))
- --breakdown and --breakdown-under, so a claim can be priced ([#618](https://github.com/AgentEnder/pristine/pull/618))
- price claims on a pool, so a breakdown stops holding the listing ([#618](https://github.com/AgentEnder/pristine/pull/618), [#602](https://github.com/AgentEnder/pristine/issues/602))
- a tree that can lose a node, and sort by more than size ([#602](https://github.com/AgentEnder/pristine/pull/602))
- the deleter can be watched, one target at a time ([#602](https://github.com/AgentEnder/pristine/pull/602))
- the rollup tree — drill in, mark a subtree, commit a batch ([#602](https://github.com/AgentEnder/pristine/pull/602))
- open the tree at a terminal, print the listing anywhere else ([#602](https://github.com/AgentEnder/pristine/pull/602))
- the terminal outside the frame ([#619](https://github.com/AgentEnder/pristine/pull/619))
- the pointer — hit-testing, click, double-click and scroll ([#620](https://github.com/AgentEnder/pristine/pull/620))
- motion that carries information ([#621](https://github.com/AgentEnder/pristine/pull/621))
- a treemap beside the tree, over the kitty graphics protocol ([#622](https://github.com/AgentEnder/pristine/pull/622))
- a label naming what the directory is, not a hint about rebuilding it ([#623](https://github.com/AgentEnder/pristine/pull/623))
- what is visible and what is selected are two different questions ([#626](https://github.com/AgentEnder/pristine/pull/626))
- a candidate may be a file, and the kind vocabulary says what losing one costs ([#652](https://github.com/AgentEnder/pristine/pull/652))
- the command line finds gitignored files, and a script must ask for the precious ones ([#652](https://github.com/AgentEnder/pristine/pull/652))
- a removal reports itself, weighs itself, and can take a spent work tree ([d83b4ec](https://github.com/AgentEnder/pristine/commit/d83b4ec))
- an exclude list, and the system's refusals told apart from real failures ([5eb214e](https://github.com/AgentEnder/pristine/commit/5eb214e))
- unreadable paths collapse to a count, and --verbose names them ([f8ae280](https://github.com/AgentEnder/pristine/commit/f8ae280))
- **release:** wire nx release across the crate and the npm packages ([3bdc4c7](https://github.com/AgentEnder/pristine/commit/3bdc4c7))

### 🩹 Fixes

- do not size what the walk pruned, and two unsafe ruleset entries ([#587](https://github.com/AgentEnder/pristine/pull/587))
- clear the git env vars that beat `-C` ([#588](https://github.com/AgentEnder/pristine/pull/588))
- three checks that failed silently toward "safe to delete" ([#588](https://github.com/AgentEnder/pristine/pull/588))
- a scan that could not read everything exits non-zero ([#588](https://github.com/AgentEnder/pristine/pull/588))
- prove the way down to a target again before removing it ([#594](https://github.com/AgentEnder/pristine/pull/594))
- remove by descriptor, never by name ([#594](https://github.com/AgentEnder/pristine/pull/594))
- prove the scan root is the directory the plan validated ([#594](https://github.com/AgentEnder/pristine/pull/594))
- do not print the reset twice in near-identical words ([#595](https://github.com/AgentEnder/pristine/pull/595))
- two control-flow windows in repo mode, found by review ([#595](https://github.com/AgentEnder/pristine/pull/595))
- judge what an entry hides, not only what it is ([#595](https://github.com/AgentEnder/pristine/pull/595))
- authenticate the tap host against pinned keys, not a scan ([#597](https://github.com/AgentEnder/pristine/pull/597))
- the by-hand tap repair reported success without committing ([#597](https://github.com/AgentEnder/pristine/pull/597))
- the tree's lifecycle — quitting, the exit status, and the terminal ([#602](https://github.com/AgentEnder/pristine/pull/602))
- join the removal however the loop ends, not just at the bottom ([#602](https://github.com/AgentEnder/pristine/pull/602))
- never set a title this run cannot put back ([#619](https://github.com/AgentEnder/pristine/pull/619))
- a gesture that costs a traversal needs a lifecycle ([#620](https://github.com/AgentEnder/pristine/pull/620))
- the loop test's quit was racing the walker ([#621](https://github.com/AgentEnder/pristine/pull/621))
- a row empties on bytes, not on a timer ([#621](https://github.com/AgentEnder/pristine/pull/621))
- an image the terminal took but would not flush ([#622](https://github.com/AgentEnder/pristine/pull/622))
- a part-emptied row must not spring back when the batch reports ([#621](https://github.com/AgentEnder/pristine/pull/621))
- the batch's position is where the deleter got to, not what worked ([#621](https://github.com/AgentEnder/pristine/pull/621))
- Unity's import cache and its compiled output are two kinds ([#623](https://github.com/AgentEnder/pristine/pull/623))
- a report the reader can be rid of ([#625](https://github.com/AgentEnder/pristine/pull/625))
- unmark reads the counts it is about to invalidate ([#626](https://github.com/AgentEnder/pristine/pull/626))
- the presets are the four that were asked for, and the axes have keys ([#626](https://github.com/AgentEnder/pristine/pull/626))
- the presets compose the two axes instead of colliding on one point ([#626](https://github.com/AgentEnder/pristine/pull/626))
- the map's change detection has to be lens-aware ([#631](https://github.com/AgentEnder/pristine/pull/631))
- the loop tests' ceiling was counted in the wrong unit ([#632](https://github.com/AgentEnder/pristine/pull/632))
- a timeout reports what it saw, it does not name a cause ([#632](https://github.com/AgentEnder/pristine/pull/632))
- a kind names what a thing is, it does not gate what a key does ([#652](https://github.com/AgentEnder/pristine/pull/652))
- a pane is reserved on what can be drawn, not on what the terminal is ([#656](https://github.com/AgentEnder/pristine/pull/656))
- the refusals a run summarises are still named, once and in order ([b036776](https://github.com/AgentEnder/pristine/commit/b036776))

### 🔥 Performance

- derive the treemap's change detection from its inputs ([#631](https://github.com/AgentEnder/pristine/pull/631))

### ❤️ Thank You

- brain loop
- Craigory Coppola @AgentEnder