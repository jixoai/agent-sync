// 镜像（加速前缀）解析：npm 启动器与 Rust 更新器共用同一套约定。
//
//   优先级：AGENTS_SYNC_MIRROR 环境变量 > 内置默认列表
//   格式：  逗号分隔的前缀列表，顺序即优先级（回退链）
//   特殊值：direct 或空值 = 直连（不加前缀）
//   拼接：  <前缀><完整 url>，前缀末尾自动补 "/"
//
// 为什么需要它：GitHub Release 在国内经常超时；社区 gh-proxy 系列能救急。
// 契约与 Rust 侧（crates/core/src/update.rs 的 mirror_prefixes）保持一致。
"use strict";

/** 内置默认列表：先社区镜像，最后官方直连兜底。 */
const DEFAULT_MIRRORS = [
  "https://gh-proxy.com/",
  "https://ghfast.top/",
  "https://ghproxy.net/",
  "", // 空串 = 直连
];

const MIRROR_ENV = "AGENTS_SYNC_MIRROR";

function normalizePrefix(prefix) {
  if (prefix === "") return "";
  return prefix.endsWith("/") ? prefix : `${prefix}/`;
}

/** 解析 `AGENTS_SYNC_MIRROR` 或默认列表，返回去重后的前缀数组（含空串兜底）。 */
function resolveMirrors(env = process.env) {
  const raw = env[MIRROR_ENV];
  const list = raw === undefined
    ? DEFAULT_MIRRORS
    : raw.split(",").map((s) => s.trim()).filter((s) => s.length > 0 || raw.includes(","));
  const out = [];
  for (const token of list) {
    const value = token.toLowerCase() === "direct" ? "" : token;
    const normalized = normalizePrefix(value);
    if (normalized !== "" && !/^https?:\/\/[^/]/i.test(normalized)) {
      throw new Error(`${MIRROR_ENV} 里的镜像前缀不是合法 URL：${JSON.stringify(token)}`);
    }
    if (!out.includes(normalized)) out.push(normalized);
  }
  if (!out.includes("")) out.push("");
  return out;
}

/** 给一个 URL 套上前缀（空串前缀即原样）。 */
function withPrefix(prefix, url) {
  return prefix === "" ? url : `${prefix}${url}`;
}

module.exports = { DEFAULT_MIRRORS, MIRROR_ENV, resolveMirrors, withPrefix };
