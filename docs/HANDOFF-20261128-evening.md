# 交接补充 — 2026-11-28 晚（容器堆槽族）

> 接 `docs/HANDOFF-20261128.md`（上午）。本轮把 ADR-0245 推到四阶段全落地，
> corpus 门禁 **187/187**（本轮起点 174）。HEAD 见 `git log origin/main -1`。

## 1. 本轮提交主题（按 ledger 切片号）

| 切片 | 内容 | 夹具 |
|---|---|---|
| 82 | closure 捕获字符串向量（closure-relay 移植收口） | closure_capture_strvec |
| 83 | 裸泛型调用复合参数推断（`box_get(&b)` 免显式实参） | generic_inference_composite |
| 84 | `Rc[T]` 共享所有权（原子句柄 + 泛型 drop 恰一次释放） | rc_shared |
| 85-86 | ADR-0245 阶段 1+2：string 布局 24/8、retag/null 准入、box_free 全 T 释放、deref-as-view | box_string, ptr_string_view |
| 87-88 | unsafe 面文档对齐 + 解释器标量 drop/帧槽 write 两臂 | （现存夹具加跑 interp） |
| 89 | fuzzer 堆字符串形状（kind 31，compiled-only 种子类） | （fuzzer） |
| 90-92 | 阶段 3：堆视图绑定（live_owned 天然成立）+ box_slot/rc_slot | ptr_string_view_bind |
| 93-94 | 阶段 4 地基+完成：vec[E] 布局 40/8、retag、VectorRef 视图链、绑定豁免 | heap_vec |
| 95 | fmt 工具追平（`#`/指针星号/extern 族/复合赋值；幂等验证） | （工具） |
| 96-97 | 索引临时量（`raw.deref()[0]`/`len(raw.deref())`）+ 连续槽 offset | heap_vec_index, heap_vec_offset |
| 98 | vec[string] 堆槽（per-family 消费形态） | heap_vec_strings |
| 100 | map 地基：布局 40/8、retag `map[K,V]`、白名单、绑定守卫 | （门位图见下） |

另有：GCC-14 fopen/LONG_MAX 修复（服务器复验 4/4）、`pub #[repr(C)]` 顺序
回归修复、rustc 1.99 四警告清零、CHANGELOG 更新（Added/Fixed/修正）、
binarytrees 基准移植。

## 2. MapPlace::Pointer —— 下一片的精确门位图

map 堆槽片的读写面卡在一个变体穿透（切片 100）：

- `MapPlace` 定义：`frontend/mod.rs:668`，`{Var(usize, MapKind), Field(usize, String, MapKind)}`
- 读者：`lower_builtin.rs:249` 的 `map_get|map_has|map_remove|map_inc|keys` 臂，
  首参 `lower_scalar_body_expression` 后按 `(Expr::Variable, Expr::Field)` 分派
- 需新增 `Pointer(String, MapKind)`（C 文本 + kind）：
  - `text()` 返回指针文本（map 调用对值位取 `&`，指位不加）
  - `slot()` 无槽——moved 追查不适用（heap 所有权在指针持有者）
- 穿透面（grep `MapPlace::Var\|MapPlace::Field` 全清单）：mod.rs（text/slot/
  f64_value）、lower_builtin.rs（读者臂：`(Expr::DerefPtr(..), t@MapOf(_))` 分
  支）、mir.rs:409（`map_place_operand`——MIR 内建渲染）、mir_pure.rs:1805、
  interp.rs:180-280/813（帧槽模型——Pointer 应为命名 trap：extern 堆块不可解）、
  closures.rs、lower_stmt.rs:377、tree.rs、movewalk.rs、codegen.rs map 调用发
  射（`&place.text()` 五处——指位去 `&`）
- 探针：`work/repro/heap_map.ae`（当前卡在读者臂的诊断
  "map_get expects a map variable or a field of one"）
- 语言先例：map 无 MapRef 拼写是设计决策（切片 99 scoping）——Pointer 是
  MIR/C 层的物化形态，不是新类型拼写

## 3. 服务器面

- `ai` 存活（并行会话保活）；`evaluate-2`/`evaluate-3` 免费层冷却（13:25Z/
  14:53Z 后可唤醒，campaign2 数据在沙箱内保留）
- 协调代理已带「禁碰本地树」约束重启；证据目标：双模式 20k seeds + 全套件
  + sanitizer → `agent/server-relay` 分支的 SERVER-STATUS.md
- ADR-0242 退役前置：5 万 seeds 双模式（本地已有 5000+），证据到齐后删
  `AE_CODEGEN=tree` 发射路径（ADR-0242 scope 节）

## 4. 队列（优先序）

1. MapPlace::Pointer 穿透（上节门位图；工作量约等于 heap_vec 切片）
2. retag 命名结构体元素（`vec[MyStruct]` 堆槽）
3. ADR-0242 退役（等服务器证据）
4. P3.4 输入绑定借用返回（`box_get[string]` 安全形态，语言特性）
5. vec/map 布局族统一（ADR-0242 之后 legacy 退场即消解大半）

## 5. 工作方式备忘（本轮新增）

- 共享文法（ae-syntax）任何一臂的改动是三件套：文法 + 路由准入 + 钉住旧诊
  断的夹具迁移——开工前 `grep -r "旧诊断文本" tests/` 圈面（22:10 条目）
- 跳过/分类键必须绑形状自身唯一标记，改完用频率计数验证覆盖率（20:25 条目）
- 两个同时失败的测试必须分别归因，不能合并回退（22:45 条目）
- 诊断前先看全量输出——head/tail 截断三次误导归因（23:59 条目）
- 协调子代理严禁碰本地树（git 写操作/编辑），文档产出走独立 clone/worktree

## 6. 追加（23:59 后）：MapPlace::Pointer 的设计分叉

开工即发现比十文件清单更深一层：map_get 的 C 发射在两层——tree 发射器直接
从 MapPlace 渲染，MIR 发射器渲染内建语句。Pointer 需携带降级 deref 表达式，
MIR 侧 lower_operand 绑定为临时；但 MapOf 无视图类型，deref 临时的绑定类型
就是 MapOf，C 发射器无法靠类型区分按值局部（&v）与指针临时（裸）。下窗二选一：

- **方案 A（推荐）**：`MapRef` 类型拼写进 Type 集（对齐 StrView/VectorRef
  先例；deref 臂、infer_place_ty、emit_place 地址 cast 各加一臂；语言一致
  性最好，兼修绑定守卫的临时形态——heap_map.ae 的绑定拒绝即可翻正）
- **方案 B**：emitter 位标记（MirBinding is_borrow_pointer），改动最小但
  类型系统留不对称

方案 A 的工作量 ≈ stage-2 字符串切片 + stage-4 向量切片之和（有完全先例可
抄）；门位图沿用本文件 §2 + 切片 100。探针 work/repro/heap_map.ae 保留
（当前卡读者臂诊断）。

## 7. 追加：MapRef 已开工（rung 1 落地）

`Type::MapRef(usize)`（map-types intern id，对齐 MapOf）已进 ae-ty，穷尽驱动
的六臂全落：ae-layout（指针布局）、ae-mir fmt（&Map#N）、ae-codegen-c
c_type_name（`ae_map *`）、c_output c_type_field（`const ae_map *`）/c_type
（声明形态）、TypeDisplay（`&map[K, V]`）。**零行为变化**（尚无生产者），
工作区 0 警告编译，已原子提交。

剩余梯队（下窗按序，每梯一提交）：
- **rung 2**：三处 deref 类型臂加 `Type::MapOf(id) => Type::MapRef(id)`
  （lower_expr 两站点 + mir.rs expr_type——照抄 String→StrView/
  Vector→VectorRef 先例）
- **rung 3**：release_var/release_member 补 MapRef no-op；lower_stmt 的
  map 绑定拒绝守卫删除（heap_map.ae 绑定形态翻正）
- **rung 4**：lower_builtin 读者臂认 `(expr, MapRef)` → MapPlace 新形态
  （或 Var 加 borrow 位）；调用点 `&` 分支按 place 类型（MapOf 取 `&`、
  MapRef 裸指针）；mir.rs:523 map_place_operand 同步
- **rung 5**：interp 帧槽模型命名 trap；heap_map.ae 三路夹具化；
  corpus floor 189；门禁全家
