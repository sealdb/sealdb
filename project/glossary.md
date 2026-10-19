# 术语表（Glossary）

> 便于人/Agent 在代码、文档、Oracle 输出之间使用同一套词汇。  
> 英文术语保留原文；中文解释以 TiDB v8.5.8 语义为准。

---

## 1. SQL / 解析

| 术语 | 含义 |
|---|---|
| **AST** | Abstract Syntax Tree，语法树。TiDB 中由 `pkg/parser/ast` 定义，`StmtNode`/`ExprNode` 为核心接口。 |
| **StmtNode** | 语句节点接口（SELECT/INSERT/DDL...）。 |
| **ExprNode** | 表达式节点接口（Column/Constant/Func...），带 `Flag`。 |
| **Flag** | 表达式属性位图：`FlagHasParamMarker`、`FlagHasFunc`、`FlagHasReference`、`FlagHasAggregateFunc`、`FlagHasSubquery`、`FlagHasVariable`、`FlagHasDefault`、`FlagPreEvaluated`、`FlagHasWindowFunc`。 |
| **Restore** | AST → SQL 文本；用于 digest、测试、Plan Cache。 |
| **Visitor** | AST 遍历模式：`Enter`/`Leave`。 |
| **Preprocess** | 解析后的语义准备：名字解析、InfoSchema 绑定、visitInfo、事务 Provider。 |
| **SQL Mode** | MySQL 兼容模式开关（如 `ONLY_FULL_GROUP_BY`、`ANSI_QUOTES`）。 |
| **Digest** | SQL 归一化指纹，用于 Plan Cache / Statement Summary。 |
| **Hint** | `/*+ ... */` 优化器提示，如 `USE_INDEX`、`HASH_JOIN`。 |
| **Conditional Comment** | MySQL `/*!50700 ... */` 条件注释。 |

## 2. 优化器

| 术语 | 含义 |
|---|---|
| **RBO** | Rule-Based Optimization，基于规则的逻辑优化。 |
| **CBO** | Cost-Based Optimization，基于代价的物理优化。 |
| **LogicalPlan** | 逻辑算子树（DataSource/Join/Agg/Selection/Projection/Sort/TopN/Limit...）。 |
| **PhysicalPlan** | 物理算子树（TableReader/HashJoin/HashAgg...）。 |
| **PlanBuilder** | AST → LogicalPlan。 |
| **optFlag** | 逻辑规则位图；第 i 位对应 `optRuleList` 第 i 条规则。 |
| **PPD** | Predicate Push Down，谓词下推。 |
| **ColumnPrune** | 列裁剪。 |
| **JoinReorder** | 连接重排序（DP/Greedy）。 |
| **Skyline Pruning** | 访问路径支配剪枝（不依赖行数估计）。 |
| **FindBestTask** | CBO 递归枚举物理实现并选代价最小。 |
| **Task** | 代价载体：Root / CopSingleRead / CopMultiRead / Mpp。 |
| **PhysicalProperty** | 物理属性：TaskTp、SortItems、ExpectedCnt、MPP 分区等。 |
| **StatsInfo** | 计划级统计：rowCount、NDV 等。 |
| **Histogram** | 直方图：Bounds/Buckets/Scalars/NDV/NullCount。 |
| **CMSketch / FMSketch** | 近似频次/NDV 草图。 |
| **TopN** | 热值统计。 |
| **Cardinality** | 基数估计。 |
| **Cost Ver1/Ver2** | TiDB 两套代价模型。 |

## 3. 执行器

| 术语 | 含义 |
|---|---|
| **Volcano** | Open/Next/Close 拉式执行模型。 |
| **Chunk** | 列存批次；`Next` 一次返回一批行。 |
| **Column** | Chunk 中的一列：null bitmap + data/offsets。 |
| **sel** | 选择向量：过滤后有效行下标；`nil` 表示全有效。 |
| **Vectorized** | 列式批处理求值（`VecEval*`），不等于手写 SIMD。 |
| **ExecutorBuilder** | PhysicalPlan → Executor 树的 `match`。 |
| **TableReader** | 构建 DAG，向 TiKV/TiFlash 发 Cop 请求并解码 Chunk。 |
| **IndexLookUp** | 索引扫描 + 回表。 |
| **PointGet / BatchPointGet** | 主键/唯一键点查快速路径。 |
| **UnionScan** | 快照结果 ∪ 事务 MemBuffer。 |
| **HashJoin / MergeJoin / IndexJoin** | 连接算法。 |
| **HashAgg / StreamAgg** | 聚合算法。 |
| **Spill** | 内存不足落盘。 |

## 4. 存储 / 分布式

| 术语 | 含义 |
|---|---|
| **PD** | Placement Driver：TSO、Region 元数据、调度。 |
| **TiKV** | 分布式 KV 存储，Raft + MVCC + Percolator 事务。 |
| **Region** | TiKV 的 KeyRange 分片单位。 |
| **RegionCache** | 客户端缓存的 Region/Leader 信息。 |
| **TSO** | Timestamp Oracle，全局时间戳。 |
| **startTS / forUpdateTS / commitTS** | 事务开始/加锁读/提交时间戳。 |
| **2PC** | Prewrite + Commit；Percolator 风格。 |
| **Pessimistic Lock** | 语句执行期加锁。 |
| **Async Commit / 1PC** | 减少提交 RTT 的优化。 |
| **Coprocessor (Cop)** | TiKV 侧执行下推算子的组件。 |
| **DAGRequest** | tipb 定义的下推执行计划。 |
| **DistSQL** | TiDB 侧构建/发送 Cop 请求的层。 |
| **MPP** | Massively Parallel Processing（TiFlash）。 |
| **Stale Read** | 历史时间戳只读。 |
| **Keyspace** | TiKV 多租户键空间（API v2）。 |

## 5. 元数据 / 事务 / 协议

| 术语 | 含义 |
|---|---|
| **InfoSchema** | 内存中的库表列索引目录。 |
| **Domain** | 单实例全局中枢：InfoSchema、DDL、Stats、SchemaValidator。 |
| **SchemaDiff** | DDL 产生的增量 schema 变更。 |
| **SchemaChecker** | 提交前校验 schema 是否变化。 |
| **MDL** | Metadata Lock，元数据锁。 |
| **MemBuffer** | 事务未提交写的内存缓冲。 |
| **LazyTxn** | 延迟激活的事务（Pending → Active）。 |
| **TxnContextProvider** | 隔离级别/TS/Schema 的钩子集合。 |
| **RR / RC / SI** | Repeatable Read / Read Committed / Snapshot Isolation。 |
| **MySQL Protocol** | 握手、COM_QUERY、结果集、OK/ERR/EOF。 |
| **Prepared Statement** | COM_STMT_PREPARE/EXECUTE/FETCH/CLOSE。 |
| **SQLSTATE** | 5 位标准错误状态码。 |

## 6. 本工程自定义

| 术语 | 含义 |
|---|---|
| **Oracle** | 用 TiDB 参考实现生成期望输出的 Go 工具。 |
| **Golden** | 由 Oracle 生成、人工 review 后的期望 JSON。 |
| **Parity** | Rust 与 TiDB 在目录/行为上的一致性。 |
| **Vertical Slice** | 一条从 SQL 输入到结果输出的最小可运行链路。 |
| **Gate** | 阶段验收门槛；不过不得进入下一阶段。 |
| **PORT_SOURCE / PORT_STATUS** | Rust 文件头中的上游路径与移植状态。 |
| **reimplemented** | 文件名对应但实现方式因语言/库差异重写。 |
| **stub / omitted** | 未实现/暂不移植，必须显式标注。 |
