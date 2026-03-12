## [0.2.3] - 2026-03-12

### 🚀 Features

- Add or_default* ResourceFuture adapters

### 🚜 Refactor

- Use adapter patterns for ResourceFuture sub-types
- [**breaking**] Move ResourceLoader generic T to associated type

### ⚙️ Miscellaneous Tasks

- Add todo reminder for future console enhancements
## [gtether_derive-v0.2.2] - 2026-02-21

### 🐛 Bug Fixes

- Drop workers sleepers lock to prevent deadlocks

### 📚 Documentation

- Add starting changelogs

### ⚙️ Miscellaneous Tasks

- Release 0.2.2
## [gtether_derive-v0.2.1] - 2026-02-20

### 🚀 Features

- Add priority-based async executor implementation
- Add worker pool
- Add FIFO executor
- Allow resource load task priorities to be configured

### 💼 Other

- Add changelog generation to release

### 🚜 Refactor

- Use priority-based executor for ResourceManager
- Move priority queue implementation to its own module
- Use worker pool for async executor
- Use worker pools to drive console command execution

### ⚙️ Miscellaneous Tasks

- Release 0.2.1
