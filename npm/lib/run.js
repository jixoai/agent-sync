// agents-sync 的 npm 启动器（参考 reply9842 的分发形态）。
//
// 这个包**不含任何二进制**：首次运行时按「包版本 + 平台」从 GitHub Release
// 下载 `agents-sync-v<ver>-darwin-arm64.tar.gz`，校验 SHA256 后缓存到
// `~/Library/Caches/agent-sync/v<ver>/`，之后直接 exec 缓存。
//
//   · npm 安装几 KB，秒级完成；
//   · 只有自己的平台付一次下载；
//   · 版本隔离缓存，升级不会串。
//
// 二进制里嵌着菜单栏 app：`agents-sync install` 会把它解到 ~/Applications。
"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const pkg = require("../package.json");
const { resolveMirrors } = require("./mirror");
const {
  checksumFor,
  downloadViaMirrors,
  fetchTextViaMirrors,
  sha256,
  sizeOf,
} = require("./download");

const REPO = "jixoai/agent-sync";
const VERSION = pkg.version;

function platformTag() {
  const osName = process.platform === "darwin" ? "darwin" : null;
  const arch = process.arch === "arm64" ? "arm64" : null;
  if (!osName || !arch) {
    throw new Error(
      `@jixoai/agent-sync 目前只支持 macOS Apple Silicon（当前 ${process.platform}/${process.arch}）`,
    );
  }
  return `${osName}-${arch}`;
}

function artifactName(tag) {
  return `agents-sync-v${VERSION}-${tag}`;
}

function releaseBase() {
  const base =
    process.env.AGENTS_SYNC_RELEASE_BASE ||
    `https://github.com/${REPO}/releases`;
  return `${base}/download/v${VERSION}`;
}

function cacheRoot() {
  if (process.env.AGENTS_SYNC_CACHE) return process.env.AGENTS_SYNC_CACHE;
  return path.join(os.homedir(), "Library", "Caches");
}

function binaryPath(tag) {
  return path.join(cacheRoot(), "agent-sync", `v${VERSION}`, "agents-sync");
}

function extractTarball(tarball, dir) {
  const { spawnSync } = require("node:child_process");
  const res = spawnSync("/usr/bin/tar", ["-xzf", tarball, "-C", dir], {
    stdio: "inherit",
  });
  if (res.status !== 0) throw new Error(`解包失败：${tarball}`);
}

/** 确保缓存里有可执行的二进制，返回路径。 */
async function ensureBinary() {
  const tag = platformTag();
  const dest = binaryPath(tag);
  if (sizeOf(dest) > 0) return dest;

  const name = artifactName(tag);
  const mirrors = resolveMirrors();
  const dir = path.dirname(dest);
  fs.mkdirSync(dir, { recursive: true });

  process.stderr.write(
    `agents-sync: 首次运行，下载 ${name}.tar.gz（版本 ${VERSION}）…\n`,
  );
  const sums = await fetchTextViaMirrors(mirrors, `${releaseBase()}/SHA256SUMS.txt`);
  const want = checksumFor(sums, `${name}.tar.gz`);
  if (!want) throw new Error(`SHA256SUMS.txt 里没有 ${name}.tar.gz`);

  const tarball = path.join(dir, `${name}.tar.gz`);
  await downloadViaMirrors(mirrors, `${releaseBase()}/${name}.tar.gz`, tarball);

  const got = sha256(tarball);
  if (got !== want) {
    fs.rmSync(tarball, { force: true });
    throw new Error(`SHA256 不匹配（期望 ${want}，实际 ${got}），已丢弃`);
  }

  const staging = path.join(dir, ".staging");
  fs.rmSync(staging, { recursive: true, force: true });
  fs.mkdirSync(staging, { recursive: true });
  extractTarball(tarball, staging);
  const bin = path.join(staging, "agents-sync");
  if (!fs.existsSync(bin)) throw new Error(`压缩包里没有 agents-sync：${tarball}`);
  fs.chmodSync(bin, 0o755);
  fs.renameSync(bin, dest);
  fs.rmSync(staging, { recursive: true, force: true });
  fs.rmSync(tarball, { force: true });
  return dest;
}

function run(bin, args) {
  const child = spawn(bin, args, { stdio: "inherit", env: process.env });
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => {
      try {
        child.kill(sig);
      } catch {}
    });
  }
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 1);
  });
  child.on("error", (e) => {
    process.stderr.write(`agents-sync: 启动失败：${e.message}\n`);
    process.exit(1);
  });
}

(async () => {
  try {
    const bin = await ensureBinary();
    run(bin, process.argv.slice(2));
  } catch (e) {
    process.stderr.write(`agents-sync: ${e.message}\n`);
    process.exit(1);
  }
})();
