//! `.aria2` 断点台账 v2：**资源身份 + 验证器 + 实际字节区间**（原子写 tmp+rename，单写者）。
//!
//! 下载中断时产物与 `.aria2` 控制文件并存；没有控制文件的残留文件必须从零重下，
//! 绝不把上次的坏文件拼进新下载。字段名沿用 aria2 的习惯方便排查。
//!
//! v1（`v:1,total,done:[段下标]`）在三种场景下会错误续传：
//! 换连接数段下标错位、同大小换内容识别不了、签名 URL 刷新不能按 URL 认资源。
//! v2 因此把身份收敛到**资源域 + 强验证器 + 长度**（强 ETag 唯一可确认；
//! Last-Modified 只作诊断），把进度收敛到**字节区间**。
//!
//! 资源域是完整请求 URL 的不可逆摘要（RFC 9110 §8.8.1：不同资源的强 validator
//! 不保证全局唯一，同 size+同 etag 也会撞）：跨 URL 一律按不同资源安全重下。
//!
//! 持久化不变量：**先保证数据落盘，再标记完成**；标记与持久化串行执行，
//! 旧快照绝不覆盖新快照（丢更新会让崩溃后把没下完的段当已完成）。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::identity::{
    covers, normalize_ranges, resource_domain, subtract, ByteRange, ResourceIdentity,
};

pub const CONTROL_VERSION: u32 = 2;

/// 台账里 `done` 区间条数上限：防御性截断，超过说明控制文件被写坏。
const MAX_DONE_RANGES: usize = 100_000;

/// 断点台账：`identity` 是资源身份、`origin` 是资源域，
/// `done` 是**已验证落盘**的字节区间（升序、不相交）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlState {
    pub v: u32,
    pub total: u64,
    pub identity: ResourceIdentity,
    /// 资源域：写入台账时的完整请求 URL 摘要（`identity::resource_domain`）。
    /// None = 旧台账没有资源域（无法证明同源）→ 续传判定一律安全重下。
    #[serde(default)]
    pub origin: Option<String>,
    pub done: Vec<ByteRange>,
    /// 台账最后更新时刻（unix 秒），排查用。
    pub updated_at: u64,
}

impl ControlState {
    pub fn new(total: u64, identity: ResourceIdentity, origin: Option<String>) -> Self {
        Self {
            v: CONTROL_VERSION,
            total,
            identity,
            origin,
            done: Vec::new(),
            updated_at: now_secs(),
        }
    }

    /// 标记区间完成并归一化（重叠/相邻合并）；越界区间裁到 total 内。
    pub fn mark_done(&mut self, range: ByteRange) {
        if range.is_empty() || self.total == 0 {
            return;
        }
        let mut ranges = std::mem::take(&mut self.done);
        ranges.push(range);
        self.done = normalize_ranges(&ranges, self.total);
        if self.done.len() > MAX_DONE_RANGES {
            self.done.truncate(MAX_DONE_RANGES);
        }
        self.updated_at = now_secs();
    }

    /// 区间是否已完整落盘。
    pub fn is_done(&self, range: ByteRange) -> bool {
        covers(&self.done, range)
    }

    /// 目标区间里还没落盘的字节（连接数变化时按区间取差集，段下标错位不影响续传）。
    pub fn pending_of(&self, target: ByteRange) -> Vec<ByteRange> {
        subtract(target, &self.done)
    }
}

/// 续传判定：台账可信且资源身份一致才续传，否则安全重下。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    Resume(Box<ControlState>),
    Restart { reason: RestartReason },
}

/// 不能续传的原因（排查进日志，不进用户可见文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// 没有 `.aria2` 台账（含只有 `.part` 的残件）。
    NoControlFile,
    /// 台账 JSON 损坏 / 字段缺失 / 区间集合写坏。
    Corrupt,
    /// 台账版本不是 v2（v1 只有段下标，无法映射到字节区间）。
    VersionMismatch,
    /// 台账 total 与当前资源长度不一致（链接换了内容）。
    TotalMismatch,
    /// `.part` 缺失。
    PartMissing,
    /// `.part` 大小与 total 不符（被截断 / 换文件）。
    PartSizeMismatch,
    /// 验证器不一致或拿不到 → 无法确认资源身份（宁可重下，不硬猜）。
    ResourceChanged,
    /// 资源域不一致：换了 URL（签名刷新 / 备用候选 / 换 CDN）或旧台账缺资源域。
    /// RFC 9110 §8.8.1：不同资源的强 validator 不保证全局唯一，域不同绝不拼旧字节。
    OriginMismatch,
}

impl RestartReason {
    /// 排查明细（只进排查日志，不参与任何状态判定）。
    pub fn detail(&self) -> &'static str {
        match self {
            Self::NoControlFile => "没有断点台账",
            Self::Corrupt => "断点台账损坏",
            Self::VersionMismatch => "断点台账版本不兼容",
            Self::TotalMismatch => "文件长度变化",
            Self::PartMissing => "半成品文件缺失",
            Self::PartSizeMismatch => "半成品文件大小不符",
            Self::ResourceChanged => "资源身份无法确认（验证器缺失或不一致）",
            Self::OriginMismatch => "资源域不一致（换 URL / 跨候选 / 旧台账缺资源域）",
        }
    }
}

/// 控制文件路径：`<dest>.aria2`（与断点续传的控制文件口径一致）。
pub fn control_path_for(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".aria2");
    PathBuf::from(name)
}

/// 分段数据落盘路径：`<dest>.part`（完成前不可见，原子 rename 交付）。
pub fn part_path_for(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// 目标路径的独占写入锁：`<dest>.lock`（OS 级 advisory 锁，进程崩溃自动释放）。
pub fn lock_path_for(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 原子写台账：唯一 tmp 名 + rename，任意时刻磁盘上都是完整 JSON。
///
/// tmp 名带 pid + 自增序号：并发段同时落盘时不会互相覆盖对方的临时文件。
pub fn save_control(dest: &Path, state: &ControlState) -> io::Result<()> {
    let control = control_path_for(dest);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        control.display(),
        std::process::id(),
        seq
    ));
    let body =
        serde_json::to_vec(state).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &control)?;
    Ok(())
}

/// 读取台账原始状态（不做续传判定，只做反序列化与结构性校验）。
pub fn load_control_state(dest: &Path) -> Result<ControlState, RestartReason> {
    let raw = fs::read(control_path_for(dest)).map_err(|_| RestartReason::NoControlFile)?;
    // 先看版本号再反序列化：v1 台账（段下标）形状不同，不能判成「损坏」，
    // 它是明确的「版本不兼容」——两者都要安全重下，但排查语义不同。
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) {
        let version = value.get("v").and_then(|v| v.as_u64());
        if version != Some(u64::from(CONTROL_VERSION)) {
            return Err(RestartReason::VersionMismatch);
        }
    }
    let state: ControlState = serde_json::from_slice(&raw).map_err(|_| RestartReason::Corrupt)?;
    if state.v != CONTROL_VERSION {
        return Err(RestartReason::VersionMismatch);
    }
    if state.total == 0 || state.done.len() > MAX_DONE_RANGES {
        return Err(RestartReason::Corrupt);
    }
    // 区间必须归一且不越界；越界/重叠说明台账被写坏，不能拿它续传。
    let normalized = normalize_ranges(&state.done, state.total);
    if normalized != state.done {
        return Err(RestartReason::Corrupt);
    }
    Ok(state)
}

/// 续传判定：`.aria2` + `.part` 同时可信，且**资源域一致 + 资源身份可确认**才续传。
///
/// 资源域（`origin` = 当前请求 URL 的摘要）不一致、旧台账缺资源域、或无法确认身份
/// （无强 ETag / 弱 ETag / 验证器不一致）一律安全重下——宁可多下一次，
/// 也不把旧字节拼进新文件（RFC 9110 §8.8.1：强 validator 不保证跨资源唯一）。
pub fn decide_resume(dest: &Path, current: &ResourceIdentity, origin: &str) -> ResumeDecision {
    if current.length == 0 {
        return ResumeDecision::Restart {
            reason: RestartReason::TotalMismatch,
        };
    }
    let state = match load_control_state(dest) {
        Ok(state) => state,
        Err(reason) => return ResumeDecision::Restart { reason },
    };
    if state.total != current.length {
        return ResumeDecision::Restart {
            reason: RestartReason::TotalMismatch,
        };
    }
    let part = match fs::metadata(part_path_for(dest)) {
        Ok(meta) if meta.is_file() => meta,
        _ => {
            return ResumeDecision::Restart {
                reason: RestartReason::PartMissing,
            }
        }
    };
    if part.len() != state.total {
        return ResumeDecision::Restart {
            reason: RestartReason::PartSizeMismatch,
        };
    }
    // 资源域必须一致：同 size+同强 etag 在不同资源间照样会撞（各自 "v1"），
    // URL 维度先于验证器把关；旧台账没有资源域也无法证明同源，一律重下。
    if state.origin.as_deref() != Some(origin) {
        return ResumeDecision::Restart {
            reason: RestartReason::OriginMismatch,
        };
    }
    if !state.identity.has_usable_validator() {
        return ResumeDecision::Restart {
            reason: RestartReason::ResourceChanged,
        };
    }
    if !current.confirms_same_resource(&state.identity) {
        return ResumeDecision::Restart {
            reason: RestartReason::ResourceChanged,
        };
    }
    ResumeDecision::Resume(Box::new(state))
}

/// 清掉半成品：`.part` 与 `.aria2` 一起删，不留会误导下次续传的残件。
///
/// 只在**资源变化 / 用户显式重来 / 单流失败 / 交付成功**时调用。
pub fn discard_partial(dest: &Path) {
    let _ = fs::remove_file(part_path_for(dest));
    let _ = fs::remove_file(control_path_for(dest));
}

/// 保留可信的半成品（暂停 / 取消 / 崩溃 / 断网 / 重试耗尽 / 永久 fs 错）。
///
/// 有意做成显式 no-op：保留矩阵的每个分支都要能在调用点读出来，
/// 而不是散落着「什么都不写」的隐式约定。
pub fn keep_partial(dest: &Path) {
    let _ = dest;
}

/// 目标路径的独占写入者锁：每个目标路径只允许一个写入者，避免不同任务/进程互相覆盖。
///
/// OS 级 advisory 锁在进程崩溃时自动释放，不留死锁；正常结束（成功或失败）时删掉锁文件，
/// 不在用户目录留下 `.lock`。崩溃留下的锁文件不持锁，下次直接复用。
///
/// 删除顺序是**先删文件、再解锁**：别人若在删除前已打开旧文件，等我们解锁后能锁住的只是
/// 已脱离路径的旧 inode；`acquire` 拿锁后会校验路径仍指向自己锁住的文件，不是就重开，
/// 因此不会出现两个写入者同时以为自己持锁。
pub struct DestLock {
    /// 只在 Drop 里取走（Windows 要先关句柄才能删文件）。
    file: Option<File>,
    path: PathBuf,
}

/// 拿锁后发现文件被换掉（并发删除）时的重开次数上限。
const LOCK_REOPEN_ATTEMPTS: usize = 4;

impl DestLock {
    pub fn acquire(dest: &Path) -> io::Result<Self> {
        let path = lock_path_for(dest);
        for _ in 0..LOCK_REOPEN_ATTEMPTS {
            let file = open_lock_file(&path)?;
            file.try_lock().map_err(|_| {
                io::Error::new(io::ErrorKind::WouldBlock, "目标路径已有另一个写入者")
            })?;
            if still_linked(&file, &path) {
                return Ok(Self {
                    file: Some(file),
                    path,
                });
            }
            // 锁住的是刚被上一位持有者删掉的旧文件：放掉它，按路径重开。
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "目标路径的锁文件被反复替换",
        ))
    }

    /// 显式释放（等价于 Drop）。
    pub fn release(self) {}
}

impl Drop for DestLock {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        release_lock_file(file, &self.path);
    }
}

fn lock_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    options
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    lock_open_options().open(path)
}

/// Unix：持锁期间先删路径、再解锁。别人此后锁住的只会是已脱离路径的旧 inode，
/// 会被 `still_linked` 识破并重开。
#[cfg(unix)]
fn release_lock_file(file: File, path: &Path) {
    let _ = fs::remove_file(path);
    let _ = file.unlock();
}

/// 打开的文件是否仍是路径当前指向的那个文件。
#[cfg(unix)]
fn still_linked(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(current)) => held.dev() == current.dev() && held.ino() == current.ino(),
        _ => false,
    }
}

/// Windows：不授予删除共享（只共享读写），任何进程还开着锁文件时删除必然失败，
/// 所以永远不会出现「锁住已删除文件」的幽灵锁。
#[cfg(windows)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    lock_open_options()
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

/// Windows：先解锁并关闭句柄，再尽力删除；别人还开着就删不掉，由最后一个持有者负责清理。
#[cfg(windows)]
fn release_lock_file(file: File, path: &Path) {
    let _ = file.unlock();
    drop(file);
    let _ = fs::remove_file(path);
}

#[cfg(windows)]
fn still_linked(_file: &File, _path: &Path) -> bool {
    true
}

/// 台账写入器：把「标记完成」与「持久化」串行化，旧快照绝不覆盖新快照。
///
/// 段并发结束时如果各自「改内存 → 复制快照 → 写盘」，后写的旧快照会丢掉别人刚标的完成段，
/// 崩溃后就会把没下完的段当已完成（错误交付）。这里用一把锁覆盖整个标记+落盘。
pub struct ControlLedger {
    dest: PathBuf,
    state: Mutex<ControlState>,
}

impl ControlLedger {
    /// 打开（或新建）台账；调用方保证此时已持有 `DestLock`。
    pub fn open(dest: &Path, total: u64, identity: ResourceIdentity, origin: &str) -> Self {
        Self::open_in_domain(dest, total, identity, &resource_domain(origin))
    }

    /// 调用方已完成重定向资源域计算，不能再把摘要当 URL 哈希第二次。
    pub(crate) fn open_in_domain(
        dest: &Path,
        total: u64,
        identity: ResourceIdentity,
        domain: &str,
    ) -> Self {
        let domain = domain.to_string();
        let state = match load_control_state(dest) {
            Ok(mut existing) if existing.total == total => {
                existing.origin = Some(domain);
                existing
            }
            _ => ControlState::new(total, identity, Some(domain)),
        };
        Self {
            dest: dest.to_path_buf(),
            state: Mutex::new(state),
        }
    }

    /// 从已判定可续传的状态打开。
    pub fn resume(dest: &Path, state: ControlState) -> Self {
        Self {
            dest: dest.to_path_buf(),
            state: Mutex::new(state),
        }
    }

    /// 标记区间完成并**立即持久化**（调用前必须保证字节已落盘）。
    pub fn mark_persisted(&self, range: ByteRange) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.mark_done(range);
        save_control(&self.dest, &state)
    }

    pub fn snapshot(&self) -> ControlState {
        self.state.lock().unwrap().clone()
    }

    pub fn pending_of(&self, target: ByteRange) -> Vec<ByteRange> {
        self.state.lock().unwrap().pending_of(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(etag: &str, length: u64) -> ResourceIdentity {
        ResourceIdentity::new(Some(etag.to_string()), None, length)
    }

    const URL: &str = "https://cdn.test/v.mp4";

    fn domain(url: &str) -> String {
        super::super::identity::resource_domain(url)
    }

    #[test]
    fn 台账记字节区间而不是段下标() {
        let mut state = ControlState::new(4096, identity(r#""v1""#, 4096), Some(domain(URL)));
        state.mark_done(ByteRange::new(100, 199));
        state.mark_done(ByteRange::new(0, 99));
        assert_eq!(
            state.done,
            vec![ByteRange::new(0, 199)],
            "相邻区间合并，连接数变化时按区间而不是下标续传"
        );
        assert!(state.is_done(ByteRange::new(50, 150)));
        assert!(!state.is_done(ByteRange::new(50, 250)));
        assert_eq!(
            state.pending_of(ByteRange::new(0, 299)),
            vec![ByteRange::new(200, 299)]
        );
    }

    #[test]
    fn 同大小换内容不能续传() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(100, identity(r#""old""#, 100), Some(domain(URL)));
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        let decision = decide_resume(&dest, &identity(r#""new""#, 100), &domain(URL));
        assert_eq!(
            decision,
            ResumeDecision::Restart {
                reason: RestartReason::ResourceChanged
            },
            "同大小换内容必须安全重下，不把旧字节拼进新文件"
        );
    }

    #[test]
    fn 弱etag台账无法确认资源身份() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(
            100,
            ResourceIdentity::new(Some(r#"W/""w""#.into()), None, 100),
            Some(domain(URL)),
        );
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        let decision = decide_resume(
            &dest,
            &ResourceIdentity::new(Some(r#"W/""w""#.into()), None, 100),
            &domain(URL),
        );
        assert!(
            matches!(decision, ResumeDecision::Restart { .. }),
            "弱 ETag 不做 If-Range 也不确认资源身份: {decision:?}"
        );
    }

    #[test]
    fn 强验证器一致才放行续传() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(100, identity(r#""same""#, 100), Some(domain(URL)));
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        match decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)) {
            ResumeDecision::Resume(state) => {
                assert_eq!(state.done, vec![ByteRange::new(0, 49)]);
                assert_eq!(
                    state.pending_of(ByteRange::new(0, 99)),
                    vec![ByteRange::new(50, 99)]
                );
            }
            other => panic!("应续传: {other:?}"),
        }
    }

    #[test]
    fn 跨url同size同etag资源域不一致必须重下() {
        // RFC 9110 §8.8.1：不同资源的强 validator 不保证全局唯一。
        // URL A 与 URL B 各自 etag="v1"、同 size：域不一致挡在验证器之前。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(
            100,
            identity(r#""v1""#, 100),
            Some(domain("https://cdn-a.test/v.mp4")),
        );
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        let decision = decide_resume(
            &dest,
            &identity(r#""v1""#, 100),
            &domain("https://cdn-b.test/v.mp4"),
        );
        assert_eq!(
            decision,
            ResumeDecision::Restart {
                reason: RestartReason::OriginMismatch
            },
            "同 size+同强 etag 但资源域不同，绝不续传"
        );
    }

    #[test]
    fn 旧台账缺资源域无法证明同源必须重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(100, identity(r#""same""#, 100), None);
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        let decision = decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL));
        assert_eq!(
            decision,
            ResumeDecision::Restart {
                reason: RestartReason::OriginMismatch
            },
            "旧台账没有资源域字段，宁可重下不硬猜"
        );
    }

    #[test]
    fn 同url且强验证器一致放行续传() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        let mut state = ControlState::new(100, identity(r#""same""#, 100), Some(domain(URL)));
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        assert!(matches!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Resume(_)
        ));
    }

    #[test]
    fn 旧版本台账安全重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        std::fs::write(control_path_for(&dest), r#"{"v":1,"total":100,"done":[0]}"#).unwrap();
        assert_eq!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Restart {
                reason: RestartReason::VersionMismatch
            },
            "v1 台账只有段下标，无法映射字节区间，必须重下"
        );
    }

    #[test]
    fn 台账损坏或区间写坏一律重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(control_path_for(&dest), "{ not json").unwrap();
        assert_eq!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Restart {
                reason: RestartReason::Corrupt
            }
        );

        // 越界区间：台账被写坏，不能拿它续传。
        let state = ControlState::new(100, identity(r#""same""#, 100), Some(domain(URL)));
        std::fs::write(
            control_path_for(&dest),
            serde_json::to_string(&ControlState {
                done: vec![ByteRange::new(0, 200)],
                ..state
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(part_path_for(&dest), vec![0u8; 100]).unwrap();
        assert_eq!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Restart {
                reason: RestartReason::Corrupt
            }
        );
    }

    #[test]
    fn part缺失或大小不符都重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let mut state = ControlState::new(100, identity(r#""same""#, 100), Some(domain(URL)));
        state.mark_done(ByteRange::new(0, 49));
        save_control(&dest, &state).unwrap();

        assert_eq!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Restart {
                reason: RestartReason::PartMissing
            }
        );

        std::fs::write(part_path_for(&dest), vec![0u8; 80]).unwrap();
        assert_eq!(
            decide_resume(&dest, &identity(r#""same""#, 100), &domain(URL)),
            ResumeDecision::Restart {
                reason: RestartReason::PartSizeMismatch
            }
        );
    }

    #[test]
    fn 并发标记完成不丢更新() {
        // 两个段几乎同时结束：旧实现「改内存→复制快照→释放锁→写盘」会让后写的旧快照
        // 丢掉别人刚标的完成段，崩溃后把没下完的段当已完成。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 4096]).unwrap();
        let ledger = std::sync::Arc::new(ControlLedger::open(
            &dest,
            4096,
            identity(r#""same""#, 4096),
            URL,
        ));

        let mut handles = Vec::new();
        for index in 0..8usize {
            let ledger = std::sync::Arc::clone(&ledger);
            handles.push(std::thread::spawn(move || {
                let start = (index * 512) as u64;
                ledger
                    .mark_persisted(ByteRange::new(start, start + 511))
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let reloaded = load_control_state(&dest).unwrap();
        assert_eq!(
            reloaded.done,
            vec![ByteRange::new(0, 4095)],
            "并发标记必须全部落盘，不得丢更新"
        );
    }

    #[test]
    fn 原子写不留tmp残件() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let mut state = ControlState::new(100, identity(r#""same""#, 100), Some(domain(URL)));
        state.mark_done(ByteRange::new(0, 9));
        save_control(&dest, &state).unwrap();

        assert!(control_path_for(&dest).exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不留 tmp 残件: {leftovers:?}");
        assert_eq!(load_control_state(&dest).unwrap(), state);
    }

    #[test]
    fn 同一目标路径只允许一个写入者() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let first = DestLock::acquire(&dest).expect("第一个写入者拿锁");
        let second = DestLock::acquire(&dest);
        assert!(
            second.is_err(),
            "第二个写入者必须被挡在门外，避免不同任务/进程覆盖同一目标"
        );
        drop(first);
        let third = DestLock::acquire(&dest);
        assert!(
            third.is_ok(),
            "锁释放后可重新获取（进程崩溃时 OS 自动释放）"
        );
    }

    #[test]
    fn 释放锁后不在用户目录留下lock文件() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let lock = DestLock::acquire(&dest).unwrap();
        assert!(lock_path_for(&dest).exists());
        drop(lock);
        assert!(
            !lock_path_for(&dest).exists(),
            "成功或失败结束都不能留 .lock"
        );
        // 崩溃留下的锁文件不持锁：可以直接复用，结束后照样清掉。
        std::fs::write(lock_path_for(&dest), b"").unwrap();
        drop(DestLock::acquire(&dest).expect("残留锁文件可复用"));
        assert!(!lock_path_for(&dest).exists());
    }

    #[cfg(unix)]
    #[test]
    fn 锁住已被删除的旧锁文件会被识破() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let holder = DestLock::acquire(&dest).unwrap();
        // 另一个写入者在持有者释放前打开了同一个锁文件……
        let stale = open_lock_file(&lock_path_for(&dest)).unwrap();
        drop(holder);
        // ……释放后它能锁住旧 inode，但那个文件已脱离路径，不能算持锁。
        stale.try_lock().unwrap();
        assert!(!still_linked(&stale, &lock_path_for(&dest)));
        let fresh = DestLock::acquire(&dest).expect("按路径重开拿到新锁");
        assert!(lock_path_for(&dest).exists());
        drop(fresh);
    }

    #[test]
    fn 保留矩阵区分保留与清理() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        std::fs::write(part_path_for(&dest), vec![0u8; 10]).unwrap();
        std::fs::write(control_path_for(&dest), "{}").unwrap();

        keep_partial(&dest);
        assert!(
            part_path_for(&dest).exists() && control_path_for(&dest).exists(),
            "暂停/断网/重试耗尽要保留可信半成品"
        );

        discard_partial(&dest);
        assert!(
            !part_path_for(&dest).exists() && !control_path_for(&dest).exists(),
            "资源变化/用户重来/单流失败才清场"
        );
    }
}
