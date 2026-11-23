//! 通过 libfontconfig 查询 Linux 上安装的字体：配置和字体列表、模式替换、匹配和排序，以及按
//! 字面名字查字族。
//!
//! Linux 上 fontconfig 是已安装字体的权威登记处：它记录了发行版安装字体的位置和用户配置的
//! 目录，也应用用户的别名和替换设置。查询用 `fontconfig` crate 的安全接口，库在运行时加载，
//! 不运行 `fc-*` 命令；库不可用时各函数返回空值，调用方退回按字体目录查找。测试以外的代码不
//! 使用 `unsafe`，只有 [`raw`] 直接调用安全接口没有覆盖的函数。
//!
//! 安全的 `Pattern::filename` 只返回 UTF-8 名字，`Pattern::get_string` 只读第一个值；本模块要
//! 保留字体路径的原始字节，也要看完整的字族列表来判断别名是否指向已安装的字体，因此在
//! [`raw`] 中补上这两项。模式写法（如 `monospace:lang=zh-cn`）由 `FcNameParse` 解析，安全接口
//! 没有这一项。

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::OnceLock;

use ::fontconfig::{
    FC_FAMILY, FC_SLANT, FC_SLANT_ITALIC, FC_WEIGHT, FC_WEIGHT_BOLD, Fontconfig, Pattern,
    UnicodeCoverage, list_fonts,
};

use super::font_path::FontFile;

/// 一次 fontconfig 匹配的结果。
pub(super) struct Match {
    /// 字体文件。
    pub(super) file: FontFile,
    /// 结果字体的字族名，用于判断查询是否真的命中；为空时按未命中处理。
    families: Vec<String>,
}

/// 选后备字体时最多尝试的 fontconfig 结果数（经验值）：系统里没有合适的字体时不逐个打开全部字体。
const SORTED_FONTS: usize = 64;

/// 必然不存在的字族名。fontconfig 对任何名字都做配置替换；用它作对照，可以区分名字命中的
/// 别名或替换规则和对任何名字都做的通用替换。
const PROBE: &str = "winc fontconfig probe 2f6d9a";

/// fontconfig 自带的通用字族名，以及它接受的旧写法。
const GENERIC: [&str; 13] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "emoji",
    "math",
    "fangsong",
    "sans",
    "sans serif",
    "mono",
    "system ui",
];

/// 返回初始化好的 fontconfig；库不可用时返回 `None`。
///
/// `FcInit` 只做一次，之后所有线程共用它载入的全局配置。测试要使用私有配置时，必须在第一次
/// 调用前通过 `FONTCONFIG_FILE` 指定，因此这类测试在子进程里运行（见 `test_fonts`）。
fn current() -> Option<&'static Fontconfig> {
    static CONFIG: OnceLock<Option<Fontconfig>> = OnceLock::new();
    CONFIG.get_or_init(Fontconfig::new).as_ref()
}

/// 返回 fontconfig 对 `pattern`（模式写法，如 `monospace:lang=zh-cn`）的最佳匹配。
pub(super) fn best(pattern: &str) -> Option<Match> {
    let mut pattern = raw::parse(current()?, pattern)?;
    let found = pattern.font_match().ok()?;
    Match::from_pattern(&found)
}

/// 返回 fontconfig 为 `pattern` 排序的字体，最匹配的在前，最多 [`SORTED_FONTS`] 个。
pub(super) fn sorted(pattern: &str) -> Vec<FontFile> {
    // 首次绘制时，字体缓存可能正在重建。沿用两秒等待上限，不在界面线程等待查询完成。
    let pattern = pattern.to_owned();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(sorted_files(&pattern));
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_default()
}

fn sorted_files(pattern: &str) -> Vec<FontFile> {
    let Some(config) = current() else {
        return Vec::new();
    };
    let Some(mut pattern) = raw::parse(config, pattern) else {
        return Vec::new();
    };
    let Ok(set) = pattern.sort_fonts(UnicodeCoverage::NoTrim) else {
        return Vec::new();
    };
    set.iter()
        .take(SORTED_FONTS)
        .filter_map(|pattern| raw::file(&pattern))
        .collect()
}

/// 返回 fontconfig 列出的全部字体文件，已排序、去重；fontconfig 不可用时返回 `None`。
pub(super) fn files() -> Option<Vec<PathBuf>> {
    let Some(pattern) = Pattern::new(current()?).ok() else {
        return Some(Vec::new());
    };
    let Ok(set) = list_fonts(&pattern, None) else {
        return Some(Vec::new());
    };
    // 空集合的 `FcFontSet.fonts` 为空指针；`FontSet::iter` 在 `nfont == 0` 时返回空迭代器，
    // 不会构造空指针切片。
    let mut paths: Vec<PathBuf> = set.iter().filter_map(|font| raw::path(&font)).collect();
    paths.sort_unstable();
    paths.dedup();
    Some(paths)
}

/// 返回 fontconfig 是否可用。
pub(super) fn available() -> bool {
    current().is_some()
}

/// 按字面字族名 `family` 查询，`bold`、`italic` 是请求的样式。
///
/// 名字是已安装的字族、fontconfig 的通用字族名，或用户配置了指向它的别名时返回结果；只是
/// fontconfig 对未知名字的后备选择时返回 `None`。
pub(super) fn lookup(family: &str, bold: bool, italic: bool) -> Option<Match> {
    let family = family.trim();
    if family.is_empty() {
        return None;
    }
    let mut matching = request(current()?, family, bold, italic)?;
    let found = Match::from_pattern(&matching.font_match().ok()?)?;
    if is_generic(family) || names_family(&found.families, family) {
        return Some(found);
    }

    // 配置替换会为别名插入目标字族。再替换一个必然不存在的名字作对照：通用的后备列表在
    // 两边相同，只有针对该名字的别名或替换会让完整列表不同。
    let mut substituted = request(current()?, family, bold, italic)?;
    substituted.config_substitute().ok()?;
    let requested = without_family(raw::families(&substituted), family);
    let mut probe = request(current()?, PROBE, bold, italic)?;
    probe.config_substitute().ok()?;
    let probe = without_family(raw::families(&probe), PROBE);
    let added = families_added(&requested, &probe);
    let installed = added
        .iter()
        .any(|name| is_generic(name) || names_family(&found.families, name));
    (!added.is_empty() && installed).then_some(found)
}

/// 返回按字面字族名 `family` 和样式建立的模式。
fn request<'a>(
    config: &'a Fontconfig,
    family: &str,
    bold: bool,
    italic: bool,
) -> Option<Pattern<'a>> {
    let mut pattern = Pattern::new(config).ok()?;
    add_string(&mut pattern, FC_FAMILY, family)?;
    if bold {
        pattern.add_integer(FC_WEIGHT, FC_WEIGHT_BOLD).ok()?;
    }
    if italic {
        pattern.add_integer(FC_SLANT, FC_SLANT_ITALIC).ok()?;
    }
    Some(pattern)
}

/// 给模式添加字符串属性。
fn add_string(pattern: &mut Pattern<'_>, name: &std::ffi::CStr, value: &str) -> Option<()> {
    let value = CString::new(value).ok()?;
    pattern.add_string(name, &value).ok()
}

impl Match {
    fn from_pattern(pattern: &Pattern<'_>) -> Option<Match> {
        Some(Match {
            file: raw::file(pattern)?,
            families: raw::families(pattern),
        })
    }
}

/// 返回查询结果的字族名中是否有 `family`。
///
/// fontconfig 比较字族名时忽略大小写和空白，但不忽略连字符、下划线：`DejaVuSans` 能找到
/// `DejaVu Sans`，`DejaVu_Sans` 找不到。
fn names_family(found: &[String], family: &str) -> bool {
    let family = family_key(family);
    found.iter().any(|name| family_key(name) == family)
}

/// 返回 `family` 是否为 fontconfig 的通用字族名或它接受的旧写法。
fn is_generic(family: &str) -> bool {
    let family = family_key(family);
    GENERIC.iter().any(|generic| family_key(generic) == family)
}

/// 返回用于比较字族名的键：去掉空白并转为小写，规则同 fontconfig。
fn family_key(family: &str) -> String {
    family
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 返回 `families` 中除字族名 `family` 以外的部分。
fn without_family(families: Vec<String>, family: &str) -> Vec<String> {
    let family = family_key(family);
    families
        .into_iter()
        .filter(|name| family_key(name) != family)
        .collect()
}

/// 返回 `requested` 中比 `probe` 多出的字族名，保留顺序和重复次数。
///
/// 别名和替换规则为请求的名字插入目标字族；通用的后备字族在两边都有，会被抵消。
fn families_added(requested: &[String], probe: &[String]) -> Vec<String> {
    let mut remaining = probe.to_vec();
    requested
        .iter()
        .filter(|family| {
            let key = family_key(family);
            match remaining.iter().position(|name| family_key(name) == key) {
                Some(at) => {
                    remaining.remove(at);
                    false
                }
                None => true,
            }
        })
        .cloned()
        .collect()
}

/// 安全接口没有覆盖、或只给出 UTF-8 名字的 fontconfig 调用。
///
/// 这些都是只读查询，除了 `FcNameParse` 返回的新模式之外不转移所有权；每个 `unsafe` 块都写明
/// 了前提。
#[allow(unsafe_code)]
mod raw {
    use std::ffi::{CStr, CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr;

    use ::fontconfig::{FC_FAMILY, FC_FILE, FC_POSTSCRIPT_NAME, Fontconfig, Pattern};
    use fontconfig_sys as sys;
    use fontconfig_sys::statics::{LIB, LIB_RESULT};

    use crate::text::font_path::FontFile;

    /// 解析 fontconfig 的模式写法。
    pub(super) fn parse<'a>(config: &'a Fontconfig, pattern: &str) -> Option<Pattern<'a>> {
        if LIB_RESULT.is_err() {
            return None;
        }
        let pattern = CString::new(pattern).ok()?;
        // SAFETY: libfontconfig 已加载，`pattern` 是以 NUL 结尾的字符串；返回值判空后才使用。
        let raw = unsafe { (LIB.FcNameParse)(pattern.as_ptr() as *const u8) };
        let raw = std::ptr::NonNull::new(raw)?;
        // SAFETY: `raw` 是 `FcNameParse` 返回的有效模式。`from_pattern` 增加一次引用，随后释放
        // `FcNameParse` 返回的那次引用，让返回的 `Pattern` 恰好拥有一次引用。
        let pattern = unsafe { Pattern::from_pattern(config, raw.as_ptr()) };
        // SAFETY: `raw` 仍是上面取得的有效模式；这次调用释放 `FcNameParse` 返回的引用，
        // 之后只由 `pattern` 持有。
        unsafe { (LIB.FcPatternDestroy)(raw.as_ptr()) };
        Some(pattern)
    }

    /// 返回模式指向的字体路径，保留非 UTF-8 字节；文件不存在时返回 `None`。
    pub(super) fn path(pattern: &Pattern<'_>) -> Option<PathBuf> {
        let path = string(pattern, FC_FILE, 0)?;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
        path.is_file().then_some(path)
    }

    /// 返回模式中的全部字族名。
    pub(super) fn families(pattern: &Pattern<'_>) -> Vec<String> {
        let mut families = Vec::new();
        for index in 0.. {
            let Some(family) = string(pattern, FC_FAMILY, index) else {
                break;
            };
            families.push(family.to_string_lossy().into_owned());
        }
        families
    }

    /// 返回模式指向的字体；只有集合文件保留下标。
    pub(super) fn file(pattern: &Pattern<'_>) -> Option<FontFile> {
        let path = path(pattern)?;
        let postscript = pattern
            .get_string(FC_POSTSCRIPT_NAME)
            .ok()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let collection = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ttc") || ext.eq_ignore_ascii_case("otc"));
        let index = pattern
            .face_index()
            .ok()
            .and_then(|index| u32::try_from(index).ok());
        Some(FontFile {
            path,
            postscript,
            index: collection.then_some(index).flatten(),
        })
    }

    /// 返回模式中 `name` 的第 `index` 个字符串值。
    fn string<'a>(pattern: &'a Pattern<'_>, name: &CStr, index: c_int) -> Option<&'a CStr> {
        if LIB_RESULT.is_err() {
            return None;
        }
        let mut value: *mut sys::FcChar8 = ptr::null_mut();
        // SAFETY: `pattern` 是有效的模式；`value` 在结果不是 `FcResultMatch` 时不被读取，匹配时
        // 指向模式所有、以 NUL 结尾的字符串，生命周期不短于 `pattern` 的借用。
        let result = unsafe {
            (LIB.FcPatternGetString)(pattern.as_ptr() as *mut _, name.as_ptr(), index, &mut value)
        };
        if result != sys::FcResultMatch || value.is_null() {
            return None;
        }
        // SAFETY: 见上；字符串以 NUL 结尾，并随模式一起释放。
        Some(unsafe { CStr::from_ptr(value as *const c_char) })
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_fonts;

#[cfg(all(test, target_os = "linux"))]
mod tests;
