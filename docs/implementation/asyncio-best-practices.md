# Python Asyncio Best Practices - Implementation Summary

## 当前使用的 Python 标准库

### 1. **asyncio.timeout()** ✅ (Python 3.11+)
**用途**: 超时控制

**代码**:
```python
if input.timeout:
    try:
        async with asyncio.timeout(input.timeout):
            async for event in stream:
                yield event
    except TimeoutError:
        # Handle timeout
        yield StreamEvent(event_type=StreamEventType.RUN_ERROR, ...)
```

**优势**:
- ✅ Context manager，自动清理
- ✅ 标准库，无需第三方依赖
- ✅ 自动取消超时的任务
- ✅ 比手动计时更高效

**替代方案**:
- ❌ `asyncio.wait_for()` - 不支持 async generator
- ❌ 手动计时 - 代码冗长，容易出错
- ❌ `async-timeout` 库 - 第三方依赖，Python 3.11+ 不需要

---

### 2. **asyncio.gather(return_exceptions=True)** ✅
**用途**: 并行执行多个任务，部分失败不影响其他任务

**代码**:
```python
tasks = [asyncio.create_task(consume_stream(i, s)) for i, s in enumerate(streams)]
results = await asyncio.gather(*tasks, return_exceptions=True)

# 检查异常
for i, result in enumerate(results):
    if isinstance(result, Exception):
        logger.error(f"Task {i} failed: {result}")
```

**优势**:
- ✅ 部分失败不取消其他任务
- ✅ 返回所有结果（包括异常）
- ✅ 适合我们的场景（多 agent 并行，独立失败）

**为什么不用 TaskGroup？**
- ❌ `asyncio.TaskGroup` - 一个任务失败会取消所有其他任务
- ❌ 不适合我们的场景（需要部分失败容忍）

**对比**:
```python
# TaskGroup - 一个失败全部取消
async with asyncio.TaskGroup() as tg:
    tg.create_task(task1())
    tg.create_task(task2())
    # 如果 task1 失败，task2 会被取消

# gather(return_exceptions=True) - 部分失败容忍
results = await asyncio.gather(
    task1(), 
    task2(), 
    return_exceptions=True
)
# task1 失败不影响 task2
```

---

### 3. **asyncio.Queue** ✅
**用途**: 协程间通信，事件多路复用

**代码**:
```python
queue = asyncio.Queue()

async def producer(idx, stream):
    async for event in stream:
        await queue.put((idx, event))
    
async def consumer():
    while True:
        item = await queue.get()
        if item is None:  # Sentinel
            break
        idx, event = item
        yield event
```

**优势**:
- ✅ 线程安全（协程安全）
- ✅ 自动阻塞/唤醒
- ✅ 无需手动同步
- ✅ 标准库，性能好

**替代方案**:
- ❌ `asyncio.wait()` - 不保证顺序，复杂
- ❌ 手动 Event/Lock - 代码冗长

---

### 4. **asyncio.CancelledError** ✅
**用途**: 取消信号传播

**代码**:
```python
except asyncio.CancelledError:
    logger.warning("Task was cancelled")
    # Cleanup
    raise  # Re-raise to propagate cancellation
```

**优势**:
- ✅ 标准取消机制
- ✅ 自动传播到子任务
- ✅ 类似 Go context cancellation

---

## 不需要的第三方库

### ❌ async-timeout
**原因**: Python 3.11+ 已有 `asyncio.timeout()`

### ❌ aiofiles
**原因**: 我们主要是网络 I/O（LLM API），不是文件 I/O

### ❌ aiometer
**原因**: 我们已经用 `asyncio.gather()` 实现了并发控制

### ❌ asyncio-pool
**原因**: 我们不需要固定大小的 worker pool，动态创建任务即可

---

## 总结

**当前实现使用的都是 Python 标准库最佳实践**：

| 功能 | 使用的库/方法 | 版本要求 | 状态 |
|------|--------------|---------|------|
| 超时控制 | `asyncio.timeout()` | Python 3.11+ | ✅ |
| 并行执行 | `asyncio.gather(return_exceptions=True)` | Python 3.7+ | ✅ |
| 事件多路复用 | `asyncio.Queue` | Python 3.7+ | ✅ |
| 取消传播 | `asyncio.CancelledError` | Python 3.7+ | ✅ |
| 任务创建 | `asyncio.create_task()` | Python 3.7+ | ✅ |

**无需第三方依赖，全部使用标准库！** 🎉

---

## Python 版本要求

- **最低**: Python 3.11 (因为使用了 `asyncio.timeout()`)
- **推荐**: Python 3.11+

如果需要支持 Python 3.7-3.10，可以：
1. 使用 `async-timeout` 库替代 `asyncio.timeout()`
2. 或者回退到手动计时（不推荐）

---

## 参考资料

- [asyncio.timeout() - Python 3.11+](https://docs.python.org/3/library/asyncio-task.html#asyncio.timeout)
- [asyncio.gather() - Python 3.7+](https://docs.python.org/3/library/asyncio-task.html#asyncio.gather)
- [asyncio.Queue - Python 3.7+](https://docs.python.org/3/library/asyncio-queue.html)
- [Structured Concurrency in Python](https://peps.python.org/pep-0789/)
