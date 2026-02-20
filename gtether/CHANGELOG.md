## [0.2.1] - 2026-02-20

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
