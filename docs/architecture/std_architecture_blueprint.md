# ae Standard Library Architecture Blueprint (M1 -> M3)

> **Document ID**: `AE-ARCH-STD-001`  
> **Status**: Accepted & Active  
> **Target Alignment**: `自主编程语言完整方案.md` (M1: Type System & Collections $\to$ M2: Self-hosting $\to$ M3: High Performance Systems)

---

## 1. 总体分层模型 (Layered Hierarchy)

ae 标准库严格遵循单向依赖规则，禁止循环引用与反向依赖：

```
+-----------------------------------------------------------------------+
|  Tier 3: std (System & Operating Environment)                          |
|  - I/O: File, TCP/UDP sockets, Environment, Process, Time Clock       |
|  - Formats: JSON, CSV, Tar, Zip, Argument Parser                      |
|  - Runtime: Thread Pool, Actor Scheduler, Signal Handling             |
+-----------------------------------------------------------------------+
                                   ▲
                                   │  (Depends on)
+-----------------------------------------------------------------------+
|  Tier 2: alloc (Heap-backed Dynamic Data Structures)                   |
|  - Collections: String, Vec, HashMap, BTreeMap, RingBuffer            |
|  - Memory Boxes: Box[T], Rc[T], Arc[T]                                |
|  - Slices & Builders: StringBuilder, ByteBuf                          |
+-----------------------------------------------------------------------+
                                   ▲
                                   │  (Depends on)
+-----------------------------------------------------------------------+
|  Tier 1: core (Zero-Allocation Foundation & Primitives)               |
|  - Types & Enums: Option[T], Result[T, E], Ordering, Pair[A, B]       |
|  - Traits: PartialEq, Clone, Hash, Display, Iterator, Bitwise, Reset   |
|  - Math & Bits: Saturating, Checked Arithmetic, Endian, VarInt, CRC   |
|  - Memory Layout: ArenaLayout, SlabBin, Watermark, AlignmentMask      |
|  - Text Primitives: AsciiTable, UTF8 boundary, StrView, Tokenizer     |
+-----------------------------------------------------------------------+
```

---

## 2. 核心架构领域矩阵 (Domain Matrix)

标准库所有模块必须严格归入以下 6 大领域，杜绝散乱实现：

### 领域 A: 基础类型与契约 (Foundation & Traits)
- **定位**: 整个语言类型系统的公理化契约。
- **核心模块**:
  - `std/core/option.ae`, `std/core/result.ae`: 可恢复错误与可选值代数类型。
  - `std/core/cmp.ae`, `std/core/clone.ae`, `std/core/hash.ae`: 核心比较、克隆与散列特征。
  - `std/alloc/default.ae`, `std/core/num.ae`, `std/core/convert.ae`: 默认重置、数值模长与显式转换。
  - `std/core/order.ae`: `Less`, `Equal`, `Greater` 全序枚举。

### 领域 B: 内存拓扑与分配策略 (Memory Topology & Allocators)
- **定位**: 为自举编译器与无 GC 高吞吐服务提供确定性内存支持。
- **核心模块**:
  - `std/core/arena.ae`, `std/core/arena_layout.ae`: 块级线性分配与内存对齐。
  - `std/core/arena_mark.ae`: 栈式作用域水位线（Watermark）批量回滚释放。
  - `std/core/arena_slab.ae`: 定长槽位 Slab 分配器 Bin 状态追踪。
  - `std/core/alignment_mask.ae`: 2 的幂次验证与内存填充（Padding）计算。
  - `std/core/slot_map.ae`: 世代索引（Generational Index），杜绝悬垂指针与野指针。

### 领域 C: 词法、语法与文本引擎 (Text & Lexing Engine)
- **定位**: 直接支撑 M2 自举编译器的前端流水线。
- **核心模块**:
  - `std/core/utf8.ae`: UTF-8 字符边界与码点字节长推导。
  - `std/core/ascii_table.ae`, `std/core/ascii_case.ae`: 极速大小写转换与忽略大小写比较。
  - `std/core/strview.ae`: 零拷贝切片子视图与边界安全截断。
  - `std/compiler/tokenizer_state.ae`: 词法分析器行号、列号与绝对偏移游标状态。
  - `std/core/char_predicate.ae`: 标识符与字面量字符谓词（`is_ident_start`, `is_ident_continue`）。
  - `std/compiler/span.ae`: 诊断源码跨度与精确定位。

### 领域 D: 算法与数据结构状态机 (Algorithms & Structural States)
- **定位**: 编译期与高频数据管道算法。
- **核心模块**:
  - `std/core/search.ae`: 二分查找（带命中与插入点索引）与线性查找。
  - `std/core/sort.ae`: 顺序断言与逆序数统计。
  - `std/core/stack.ae`, `std/core/queue.ae`: 定长无锁栈与循环队列索引状态。
  - `std/core/probe.ae`: 开放寻址法哈希表的线性与二次探测步进。
  - `std/core/bloom.ae`: 64 位定长布隆过滤器位集状态。

### 领域 E: 序列化、编码与校验 (Serialization & Integrity)
- **定位**: 编译器字节码、缓存序列化与网络协议编解码。
- **核心模块**:
  - `std/core/varint.ae`, `std/core/leb128_decode.ae`: LEB128 可变长整数编解码状态机。
  - `std/core/endian_safe.ae`: 显式大端序/小端序拆装。
  - `std/core/crc.ae`, `std/core/crc8.ae`, `std/core/checksum.ae`: 完整性校验循环冗余码。
  - `std/core/base64.ae`, `std/core/hex.ae`, `std/core/hex_printer.ae`: 文本数据编解码。
  - `std/core/int_formatter.ae`: 无堆分配的十进制整数 ASCII 格式化器。

### 领域 F: 算术安全与系统度量 (Arithmetic Safety & Metrics)
- **定位**: 保证数学计算不发生未定义行为，支撑长时间监控。
- **核心模块**:
  - `std/core/checked_arithmetic.ae`: 溢出检查加减法。
  - `std/core/saturating.ae`: 边界饱和截断算术。
  - `std/core/sliding_window.ae`: 4 槽位滑动窗口移动累加与平均值。
  - `std/core/exponential_decay.ae`: 整数 EMA 指数平滑估计器。
  - `std/core/counter.ae`: 成功/失败非负事件计数器与千分比错误率。
  - `std/core/simd_lane.ae`, `std/core/simd_compare.ae`, `std/core/simd_shuffle.ae`: 4 通道概念 SIMD 运算、掩码与洗牌。

---

## 3. 标准化设计与编码规范 (Enforced Invariants)

所有纳入标准库的代码必须严格满足以下规范：
1. **零 BOM 规则**: 源码必须为 UTF-8 无 BOM 编码，避免词法分析器 `\u{feff}` 报错。
2. **所有权防重复移动 (No Use-After-Move)**: 结构体作为值传递后，禁止在同作用域二次使用；统一使用不可变借用 `&T` 或重新构造新实例。
3. **参数命名避免关键字**: 禁止使用 `len`, `bytes` 等内置内建标识符作为参数名或局部变量名。
4. **整数防溢出设计**: 严禁编写超出正 $2^{63}-1$ 范围的常量乘法或无保护位移，跨度计算与哈希结合必须包含模数或饱和防线。
5. **配套 ADR 与快照测试**: 每一组新模块必须同步提交对应的 `docs/adr/01xx-*.md` 决策文档以及 `tests/run/batch_core_*.ae` 运行快照测试。
