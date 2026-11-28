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

## 8. 追加：rung 1-3 已落地，rung 4 的设计深度

rung 2（四处 deref 类型臂 MapOf→MapRef：lower_expr 两站点 + mir expr_type +
pure infer_place_ty）与 rung 3（release no-ops + 绑定守卫退役）已提交。探针
推进到读者臂诊断——按计划。

rung 4 开工发现：`MapPlace` 是语义位（slot id），text() 从 id 派生；Pointer
的语义内容是**表达式**（deref 的 C 文本只在发射期存在）。两难：
- 携带 `Box<Expr>`：text() 无法工作（需发射器），map 调用五处的
  `&place.text()` 需重排为 `emit_expr` 分支（值位取 `&`、指位裸）
- 携带 C 文本：MIR 降级期（mir.rs:523）文本不存在

**推荐路径**：MapPlace::Pointer(Box<Expr>, MapKind) + 五处调用点改
`emit_expr` 分支（tree 发射器内 map_get/has/inc/remove/keys 各一处，
`&` 按表达式类型分支：MapOf→`&`、MapRef→裸）+ moved 检查对 Pointer 跳过。
MIR 路由：mir.rs:523 的 map_place_operand 对 Pointer 走
lower_operand(expr)（临时绑定，deref_ptr 内建渲染已存在）。interp 三臂命名
trap（extern 堆块不可解）。工作量 ≈ stage-4 向量切片；先例 heap_vec_index
（未绑定链的物化临时 + 消费分支）完全同构。

## 9. 更正：interp 无需 MapRef 臂

§7 的「interp 三臂命名 trap」判断过重。实测分析：interp 的 DerefPtr 求值
按槽位取值、不对结果类型分支（`Val::Map` 原样返回）；MapRef 只是编译期类型
标记，interp 值域里不可见。且堆 map 指针在帧槽模型中本不可构造（extern
alloc 陷阱）——interp 对 map 堆槽形状保持现有行为即正确，rung 4 的 interp
工作量归零。§8 的三臂清单相应缩减为：读者臂 + MapPlace 表达式形态 + 五调
用点 + mir.rs:523。rung 4 剩余面比 §8 估计再小一档。

## 10. 追加：槽位守卫已认 MapRef；探针钉在读者门

lower_body 的 slots_supported 家族加 MapRef（借位不移动，与 StructRef/
VectorRef 同列）。`heap_map_bind.ae`（绑定形态：let view = raw.deref() 后
map_has(view, ...)）现精确停在 rung 4 核心——"map_has expects a map
variable or a field of one"。探针文件保留于 work/repro/（gitignore，按本
文件内容重建即可）。rung 4 落地后此探针应三路通过并夹具化。

## 11. 追加：服务器战役状态（23:59 后）

- evaluate-2 已苏醒（冷却解除）。ae-r2（旧树 04dff421+partial）的 campaign2：
  套件段 49 二进制全过（repr(C) 测试除外——该树在修复前），pure-subset 段随
  沙箱休眠中断，其残留进程已按 PID 清理。
- **ae-r3（当前 main 全量 56 提交）campaign3**：套件段 **49/49 全绿**
  （suite.log 留存），双模式 20k fuzz 已起（进程 1688708 链）。
- **ae-relay（协调代理部署）**：pure-subset 20k + duration 1h mixed 双战役
  自跑（进程 1056623/1535022）。
- 4 核主机上双树三战役并行——种子总额归并时按树版本标注（ae-r3=ec6aa60f+
  段；ae-relay=代理部署时点段）。收割下窗进行。

## 12. 追加：campaign3 中期状态

套件段 **49/49 全绿**（suite.log）。pure-subset 段跑至 seed ~425 全 ok 后被
沙箱休眠终止（无汇总行）；mixed 段正跑（seed 400+，持续 ok）。收割时：
mixed 段汇总即 mixed 证据；pure-subset 需重跑（`cd /data/home/ae-r3 &&
python3 scripts/diff_fuzz.py --pure-subset --seeds 20000 --jobs 2`，混合段
完成后）。ADR-0242 前置的本地+服务器套件绿已齐；5 万种子目标按双模式累计
（本地 900 + 服务器段）。

## 13. 本地 fuzz 总额（ADR-0242 账目）

本会话本地段（均为当前 main 或近版，0 失败）：mixed 300（kind 31 首验）+
pure 300（同）+ mixed 200 + pure 200（修正键后）+ mixed 100 + pure 100（发
射器臂后冒烟）+ mixed 100 + pure 100（写穿/MapRef 后冒烟）= **mixed 900 +
pure 900 = 1800 seeds 本地**。历史段（先前会话）：pure-subset 5000 + 各冒烟
~1000。ADR-0242 的 50k 双模式目标：服务器段（relay + campaign3 + 哨兵）累
计为主，本地 1800 为冒烟层。relay 段被沙箱周期性 Terminated——按段累计、
汇总行缺失时以日志 ok 行计数为准。

## 14. s2 FAILURE 簇 triage 定论

relay s2 pure-subset 的 ~678 行 FAILURE（seed 1987 起成簇）经复跑证伪：
seed 1987 在其树双后端当下全过——为超载主机（双树战役+其他会话负载）下的
编译超时误计，与 11-19 事故同源。干净指标=ok 行计数；非超时真分歧=0 已知。
已转达 relay 代理（记账指引 + 轮询改进建议：超时种子复跑一次再计数）。
ADR-0242 证据账：本地 1800 冒烟 + 服务器 ok 行累计 + 双主机套件绿 + 消毒器
48 夹具全过——退役决策材料完整，等 relay 总额推送后执行。

## 15. P3.4 邻接切片门位（探针实测）

源级 `&map[string, i64]` 参数拼写当前拒绝，诊断即白名单：
"unsupported borrow type; write &string, &vec, &vec[string], &vec[f64],
&Struct, &[i64; N], &mut [T], or &T inside a generic function"。
切片路径（先例 &vec→VectorRef 完整）：
1. 共享文法 borrow 白名单加 `&map[K, V]`（诊断文本同步）
2. 参数类型解析为 `Type::MapRef(id)`（rung 1 已就位）
3. 槽位守卫已认（rung 3）；map 调用读者已认 `(Variable, MapRef)`→VarBorrow（rung 4）
4. 调用点借用实参准入（`total(&counts)` 的 Argument 形态——vec 先例 View）
5. MIR/pure/interp 的 MapRef 参数渲染（指针形态）
探针：work/repro/map_sig.ae。工作量 ≈ stage-4 向量切片。

## 16. P3.4 地基已落：&map[K, V] 参数拼写

文法 borrow 白名单加 `&map[K, V]`（types.rs，键值经 named_type 解析、
intern_map_types intern）+ lower_body 参数白名单认 MapRef（槽位守卫
rung 3 已认）。探针 work/repro/map_sig.ae 推进至调用实参门：
"argument 1 to 'total' must be &map[string, i64]"——调用方写
`total(&counts)` 后撞借用实参分类器（&map 形态未进 parse_borrow_argument
/Argument 分类——值参数 `total(counts)` 也被拒因参数是借位）。剩余两门：
借用实参分类（Argument::View 的 map 形态，&vec 先例）+ 被调端 place
（MapPlace::VarBorrow 已在，参数绑定成 MapRef 局部即落）。工作量小，
下窗可完。纯增量地基已提交（零行为变化，门禁全绿）。

## 17. 追加：调用点白名单+borrows_place 已落

lower_expr.rs 两处：BorrowArgument 拒绝白名单加 MapRef 参数；borrows_place
匹配加 MapRef↔MapOf 同 id 判定（Argument::Value(BorrowAs(place)) 形态，
&vec 同构）。探针推进至扫描准入门："this expression is not supported by
the body route"（scan 的借用实参形状检查未认 map 借位）。剩余单门：
lower_scan 的借用实参准入加 map 形态（MapOf 局部被 & 借用时视作 VarBorrow
实参）→ 三路夹具化。工作量小，先例 &vec 准入在同文件。

## 18. 追加：借用实参分类已落，降级路径 catch-all 为剩余门

lower_expr.rs 两处（BorrowArgument 拒绝白名单 + borrows_place 的
MapRef↔MapOf 判定 → Argument::Value(BorrowAs(place)) 形态）已提交。
探针再进：参数构建成功后，`&counts` 表达式在某路径被
lower_scalar_body_expression 直接降级触 catch-all（lower_expr.rs:1189
"this expression is not supported by the body route"）。下窗追踪：
(1) 找出哪条路径对 BorrowArgument 再降级（print 臂预降级/values 集合/
Argument::Value 的 emit 前降级）；(2) 该路径应让 BorrowAs(MapOf place)
走 BorrowAs 发射臂（`&(v)` 形态已存在）。先例：&vec 参数全链同构。
零 corpus 影响（无既有 MapRef 参数形状），门禁全绿已提交。

## 19. §18 线索补充：borrow-argument 分支对 map 基底提前 bail

resolve_borrow_place_type 对空 fields 的 MapOf 基底已透传（无需改）。真门在
borrow-argument 分支更早处：`&counts` 实参进入分类前，place 解析入口
（receiver_field_type / resolve_field_chain 对 map 局部的处理）或分支的
前置类型门对 MapOf 基底 bail → 落回标量降级 → catch-all。下窗从
lower_value_arguments 的 BorrowArgument 臂入口单步追踪（80 行），找到
bail 点后加 map 形态即可。树已恢复 HEAD 态（误编辑全回退，零残留）。
