// 断点续传下载 + SHA256 校验（只用 node 内置模块）。
//
// 为什么认真对待下载：二进制被截断时 `file` 仍会说它是可执行文件，真跑起来才
// 崩。这里三道门：字节数对得上、SHA256 对得上、校验通过前不改名成正式产物。
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const http = require("node:http");
const https = require("node:https");

const IDLE_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;

function request(url, headers = {}, redirects = 0) {
  return new Promise((resolve, reject) => {
    const mod = url.startsWith("https:") ? https : http;
    const req = mod.get(url, { headers }, (res) => {
      const { statusCode, headers: h } = res;
      if (statusCode >= 300 && statusCode < 400 && h.location) {
        res.resume();
        if (redirects >= MAX_REDIRECTS) return reject(new Error(`重定向过多：${url}`));
        const next = new URL(h.location, url).toString();
        return resolve(request(next, headers, redirects + 1));
      }
      resolve({ status: statusCode, headers: h, stream: res });
    });
    req.setTimeout(IDLE_TIMEOUT_MS, () => req.destroy(new Error("下载空闲超时")));
    req.on("error", reject);
  });
}

async function fetchText(url) {
  const res = await request(url, { "user-agent": "agents-sync-launcher" });
  if (res.status !== 200) {
    res.stream.resume();
    throw new Error(`HTTP ${res.status}：${url}`);
  }
  let text = "";
  for await (const chunk of res.stream) {
    text += chunk;
    if (text.length > 1_000_000) throw new Error(`响应过大：${url}`);
  }
  return text;
}

function sizeOf(file) {
  try {
    return fs.statSync(file).size;
  } catch {
    return 0;
  }
}

/**
 * 把 `url` 下载到 `dest`，中断后续传。`expectedBytes` 已知时做最终核对。
 * 成功时 `dest` 是完整文件。
 */
async function downloadResumable(url, dest, expectedBytes = 0) {
  const part = `${dest}.part`;
  for (let attempt = 1; attempt <= 3; attempt++) {
    const existing = sizeOf(part);
    const headers = { "user-agent": "agents-sync-launcher" };
    if (existing > 0) headers.range = `bytes=${existing}-`;
    const res = await request(url, headers);

    if (res.status === 416) {
      // 已下完（或 .part 比远端还长）：交给下面的长度核对处理
      res.stream.resume();
      break;
    }
    if (res.status !== 200 && res.status !== 206) {
      res.stream.resume();
      throw new Error(`HTTP ${res.status}：${url}`);
    }

    // 206 必须从已有长度接上，否则这段 body 不可用，清空重来
    let start = 0;
    if (res.status === 206) {
      const m = /bytes\s+(\d+)-/i.exec(res.headers["content-range"] || "");
      start = m ? Number(m[1]) : -1;
      if (start !== existing) {
        res.stream.resume();
        fs.rmSync(part, { force: true });
        continue;
      }
    } else if (existing > 0) {
      fs.rmSync(part, { force: true });
    }

    await new Promise((resolve, reject) => {
      const out = fs.createWriteStream(part, { flags: start > 0 ? "a" : "w" });
      res.stream.pipe(out);
      res.stream.on("error", reject);
      out.on("error", reject);
      out.on("finish", resolve);
    });

    if (expectedBytes > 0 && sizeOf(part) !== expectedBytes) {
      fs.rmSync(part, { force: true });
      continue;
    }
    fs.renameSync(part, dest);
    return dest;
  }
  if (expectedBytes > 0 && sizeOf(part) === expectedBytes) {
    fs.renameSync(part, dest);
    return dest;
  }
  throw new Error(`下载不完整：${url}（${sizeOf(part)}/${expectedBytes || "?"} 字节）`);
}

/** 依次尝试镜像前缀，直到某一个成功。 */
async function downloadViaMirrors(mirrors, url, dest, expectedBytes = 0) {
  const errors = [];
  for (const prefix of mirrors) {
    const full = prefix === "" ? url : `${prefix}${url}`;
    try {
      return await downloadResumable(full, dest, expectedBytes);
    } catch (e) {
      errors.push(`${prefix || "direct"}: ${e.message}`);
    }
  }
  throw new Error(`全部镜像失败：\n  ${errors.join("\n  ")}`);
}

async function fetchTextViaMirrors(mirrors, url) {
  const errors = [];
  for (const prefix of mirrors) {
    const full = prefix === "" ? url : `${prefix}${url}`;
    try {
      return await fetchText(full);
    } catch (e) {
      errors.push(`${prefix || "direct"}: ${e.message}`);
    }
  }
  throw new Error(`全部镜像失败：\n  ${errors.join("\n  ")}`);
}

function sha256(file) {
  const h = crypto.createHash("sha256");
  h.update(fs.readFileSync(file));
  return h.digest("hex");
}

/** 从 `SHA256SUMS.txt` 里取某个文件名的哈希。 */
function checksumFor(text, name) {
  for (const line of text.split("\n")) {
    const m = /^\s*([0-9a-f]{64})\s+\*?(.+?)\s*$/i.exec(line);
    if (m && path.basename(m[2]) === name) return m[1].toLowerCase();
  }
  return null;
}

module.exports = {
  checksumFor,
  downloadResumable,
  downloadViaMirrors,
  fetchText,
  fetchTextViaMirrors,
  sha256,
  sizeOf,
};
