// agents-sync 菜单栏面板（产品形态参考 reply9842：托盘是本体，窗口只是面板）。
//
// 三块职责：
//   1. 托盘：常驻菜单栏；菜单里带**实时状态行**（禁用项），并且永远有「检查更新」。
//   2. 面板：一眼看清状态（设备/目录/最近一次同步），动作就地完成。
//   3. 设置：同步参数与更新开关直接改，不逼用户去编配置文件。
//
// 数据全部来自 Rust（`asui/native`）：运行态走 daemon 的 unix socket，
// 设备清单走 devices.json，配置走 config.toml / config.local.toml。
//
// nsh 的反应式规则（踩坑后）：组件体只跑一次，信号必须在 JSX 表达式里读，
// 所以视觉树放在 `content()`；信号变化时它整体重跑。
import { Clipboard, mount, signal, Text, View, token } from "nsh";
import { Button } from "nsh/base";
import { finish } from "./lib/ui/theme";
import type { MenuItem } from "nsh/desktop";
import { Application, Tray, Window, displays } from "nsh/desktop";
import trayIcon from "./assets/tray.svg";
import { Sync } from "asui/native";

const T = {
  bg: () => token("background"),
  panel: () => token("card"),
  panel2: () => token("muted"),
  stroke: () => token("border"),
  text: () => token("foreground"),
  text3: () => token("muted-foreground"),
  accent: () => token("primary"),
  onAccent: () => token("primary-foreground"),
  ok: () => token("up"),
  warn: () => token("warn"),
  danger: () => token("destructive"),
};

type Peer = { id: string; label: string; lan: string; ticket: string; last: "ok" | "failed" | "unknown" };
type Tree = { name: string; source: string; payload: string; enabled: boolean; files: number };
type Last = {
  at: string;
  captured: number;
  committed: boolean;
  applied_files: number;
  removed_files: number;
  conflicts: string[];
  failed_fetch: [string, string][];
  fetched: string[];
  error: string | null;
} | null;
type Overview = {
  ok: boolean;
  error?: string;
  device: string;
  label: string;
  version: string;
  update_available: string | null;
  config_path: string;
  jj_version: string;
  daemon: { running: boolean; paused: boolean; syncing: boolean; pid: number | null };
  last: Last;
  sync: { tick_interval: number; watch_cooldown: number; backup_keep: number; prune: boolean };
  update_settings: { auto: boolean; check_interval: number };
  peers: Peer[];
  trees: Tree[];
};

function Dot(props: { color: string }) {
  return <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: props.color }} />;
}

function Stat(props: { label: string; value: string; unit?: string }) {
  return (
    <View
      style={{
        flex: 1,
        padding: 10,
        borderRadius: 10,
        backgroundColor: T.panel2(),
        borderWidth: 1,
        borderColor: T.stroke(),
      }}
    >
      <Text style={{ fontSize: 10.5, color: T.text3() }}>{props.label}</Text>
      <View style={{ flexDirection: "row", alignItems: "flex-end", gap: 3 }}>
        <Text style={{ fontSize: 19, fontWeight: "600", color: T.text() }}>{props.value}</Text>
        {props.unit ? <Text style={{ fontSize: 11, color: T.text3() }}>{props.unit}</Text> : null}
      </View>
    </View>
  );
}

/** 设计稿里的胶囊开关。 */
function Toggle(props: { on: boolean; onChange: (v: boolean) => void }) {
  return (
    <Button
      onPress={() => props.onChange(!props.on)}
      accessibilityLabel={props.on ? "关闭" : "开启"}
      style={{
        width: 30,
        height: 18,
        borderRadius: 999,
        backgroundColor: props.on ? T.accent() : "rgba(138,138,148,0.35)",
        justifyContent: "center",
        padding: 2,
        alignItems: props.on ? "flex-end" : "flex-start",
      }}
    >
      <View style={{ width: 14, height: 14, borderRadius: 7, backgroundColor: "#ffffff" }} />
    </Button>
  );
}

/** 设置项：标签 + 当前值 + 减/加。步进而不是文本框：不用键盘、不会输错。 */
function Stepper(props: {
  label: string;
  hint?: string;
  value: number;
  unit: string;
  onDelta: (d: number) => void;
  disabled?: boolean;
}) {
  const btn = (label: string, delta: number) => (
    <Button
      onPress={() => props.onDelta(delta)}
      disabled={props.disabled}
      style={{
        width: 24,
        height: 22,
        borderRadius: 7,
        backgroundColor: T.panel2(),
        borderWidth: 1,
        borderColor: T.stroke(),
        alignItems: "center",
        justifyContent: "center",
      }}
    >
      <Text style={{ fontSize: 13, color: T.text() }}>{label}</Text>
    </Button>
  );
  return (
    <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingVertical: 6 }}>
      <View style={{ flex: 1 }}>
        <Text style={{ fontSize: 12, color: T.text() }}>{props.label}</Text>
        {props.hint ? <Text style={{ fontSize: 10.5, color: T.text3() }}>{props.hint}</Text> : null}
      </View>
      <Text style={{ fontSize: 12, fontWeight: "600", color: T.text() }}>
        {props.value + " " + props.unit}
      </Text>
      {btn("−", -1)}
      {btn("+", 1)}
    </View>
  );
}

function ago(iso: string | undefined): string {
  if (!iso) return "尚未同步";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return iso;
  const s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return `${s}s 前`;
  if (s < 3600) return `${Math.round(s / 60)} 分钟前`;
  return `${Math.round(s / 3600)} 小时前`;
}

function App() {
  const ov = signal<Overview | null>(null);
  const err = signal<string>("");
  const busy = signal(false);
  const tab = signal<"panel" | "settings">("panel");
  const note = signal<string>("");
  const checking = signal(false);

  let noteTimer: number | undefined;
  const say = (text: string) => {
    note.set(text);
    if (noteTimer) clearTimeout(noteTimer);
    noteTimer = setTimeout(() => note.set(""), 6000);
  };

  const load = () => {
    try {
      const next = JSON.parse(Sync.overview()) as Overview;
      ov.set(next);
      if (!next.ok) err.set(next.error ?? "读取失败");
      else err.set("");
      syncTray(next);
    } catch (e) {
      err.set(String(e));
    }
  };

  setTimeout(load, 0);
  setInterval(load, 4000);

  const act = (cmd: "sync" | "pause" | "resume") => {
    busy.set(true);
    try {
      const r = JSON.parse(Sync.action(cmd)) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "操作失败");
      else {
        err.set("");
        say(cmd === "sync" ? "已触发同步" : cmd === "pause" ? "已暂停自动同步" : "已恢复自动同步");
      }
    } catch (e) {
      err.set(String(e));
    }
    load();
    setTimeout(() => {
      busy.set(false);
      load();
    }, 800);
  };

  const toggleTree = (name: string, enabled: boolean) => {
    try {
      const r = JSON.parse(Sync.setTreeEnabled(name, enabled)) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "切换失败");
      else say(`${name} 已${enabled ? "启用" : "暂停"}，下一轮生效`);
    } catch (e) {
      err.set(String(e));
    }
    load();
  };

  const setSync = (patch: Record<string, number | boolean>) => {
    try {
      const r = JSON.parse(Sync.setSync(JSON.stringify(patch))) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "保存失败");
      else say("设置已保存");
    } catch (e) {
      err.set(String(e));
    }
    load();
  };

  const setUpdate = (patch: Record<string, number | boolean | string>) => {
    try {
      const r = JSON.parse(Sync.setUpdate(JSON.stringify(patch))) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "保存失败");
      else say("设置已保存");
    } catch (e) {
      err.set(String(e));
    }
    load();
  };

  const checkUpdate = () => {
    checking.set(true);
    say("正在检查更新…");
    try {
      const r = JSON.parse(Sync.checkUpdate()) as {
        ok: boolean;
        error?: string;
        current?: string;
        latest?: string;
        newer?: boolean;
      };
      if (!r.ok) {
        err.set(r.error ?? "检查更新失败");
        say("");
      } else if (r.newer) {
        say(`发现新版本 ${r.latest}（当前 ${r.current}）`);
        load();
      } else {
        say(`已是最新版本 v${r.current}`);
      }
    } catch (e) {
      err.set(String(e));
    }
    checking.set(false);
  };

  const doUpdate = () => {
    say("正在下载并安装更新，完成后界面会重启…");
    try {
      const r = JSON.parse(Sync.update()) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "更新失败");
    } catch (e) {
      err.set(String(e));
    }
    load();
  };

  const copyPath = (path: string) => {
    Clipboard.writeText(path)
      .then(() => say("路径已复制：" + path))
      .catch((e) => err.set(String(e)));
  };

  const openConfigFile = () => {
    try {
      const r = JSON.parse(Sync.openConfig()) as { ok: boolean; error?: string };
      if (!r.ok) err.set(r.error ?? "打开失败");
      else say("已在编辑器中打开 config.toml");
    } catch (e) {
      err.set(String(e));
    }
  };

  // ---- 托盘 ----
  const PANEL_W = 380;
  const PANEL_H = 500;
  const panelWin = () => Window.get("main");
  let traySig = "";

  const hidePanel = () => {
    panelWin()?.hide();
  };

  const showPanel = (at: { origin: { x: number; y: number }; size: { width: number; height: number } } | null) => {
    const w = panelWin();
    if (!w) return;
    if (at) {
      let x = at.origin.x + at.size.width / 2 - PANEL_W / 2;
      let y = at.origin.y + at.size.height + 6;
      const list = displays();
      const d =
        list.find((dd) => at.origin.x >= dd.bounds.origin.x && at.origin.x <= dd.bounds.origin.x + dd.bounds.size.width) ??
        list[0];
      const vb = d?.visibleBounds;
      if (vb) {
        x = Math.min(Math.max(x, vb.origin.x + 6), vb.origin.x + vb.size.width - PANEL_W - 6);
        y = Math.min(y, vb.origin.y + vb.size.height - PANEL_H - 6);
      }
      w.setBounds({ origin: { x: Math.round(x), y: Math.round(y) }, size: { width: PANEL_W, height: PANEL_H } });
    }
    w.show();
    w.focus();
  };

  // 菜单带状态行（禁用项），并且只在文案变化时重建——
  // 每 4 秒重建一次正在弹出的菜单会让它闪烁。
  const syncTray = (d: Overview | null) => {
    if (!d) return;
    const st = d.daemon;
    const last = d.last;
    const statusLine = !st.running
      ? "● daemon 未运行"
      : st.syncing
        ? "◐ 同步中…"
        : st.paused
          ? "⏸ 已暂停"
          : last?.at
            ? `● 已同步 · ${ago(last.at)}`
            : "● 已就绪 · 尚未同步";
    const peerLine =
      d.peers.length === 0
        ? "尚未接入设备"
        : d.peers
            .map((p) => `${p.label || p.id} ${p.last === "ok" ? "已同步" : p.last === "failed" ? "拉取失败" : "待同步"}`)
            .join(" · ");
    const statsLine = `采集 ${last?.captured ?? 0} · 落盘 ${last?.applied_files ?? 0} · 冲突 ${last?.conflicts?.length ?? 0}`;
    const upd = d.update_available;
    const sig = [statusLine, peerLine, statsLine, upd ?? "", st.paused ? 1 : 0, st.running ? 1 : 0].join("|");
    if (sig === traySig) return;
    traySig = sig;

    const menu: MenuItem[] = [
      { label: "打开面板", onSelect: () => { tab.set("panel"); showPanel(null); } },
      { type: "separator" },
      { label: statusLine, disabled: true },
      { label: peerLine, disabled: true },
      { label: statsLine, disabled: true },
      { type: "separator" },
      { label: "立即同步", onSelect: () => act("sync") },
      { label: st.paused ? "继续同步" : "暂停同步", onSelect: () => act(st.paused ? "resume" : "pause") },
      { type: "separator" },
      upd
        ? { label: `安装更新 ${upd}`, onSelect: doUpdate }
        : { label: "检查更新…", onSelect: checkUpdate },
      { label: "设置…", onSelect: () => { tab.set("settings"); showPanel(null); } },
      { type: "separator" },
      { label: "退出 agents-sync", role: "quit" },
    ];
    Tray.set({ icon: trayIcon, tooltip: "agents-sync", menu, onClick: (e) => {
      if (panelWin()?.state()?.visible) hidePanel();
      else showPanel(e.bounds);
    } });
  };

  panelWin()?.on("focusChange", (ev) => {
    if (!ev.focused) hidePanel();
  });
  panelWin()?.on("closePress", () => hidePanel());

  // ---- 视觉树 ----
  const content = () => {
    const d = ov();
    const e = err();
    const isBusy = busy();
    const tc = tab();
    const hint = note();

    const running = d?.daemon.running ?? false;
    const paused = d?.daemon.paused ?? false;
    const syncing = d?.daemon.syncing ?? false;
    const last = d?.last ?? null;
    const conflicts = last?.conflicts?.length ?? 0;
    const peers = d?.peers ?? [];
    const trees = d?.trees ?? [];
    const upd = d?.update_available;

    const dot = !running ? T.text3() : conflicts > 0 ? T.danger() : syncing ? T.warn() : T.ok();
    const headline = !running
      ? "daemon 未启动"
      : syncing
        ? "同步中…"
        : paused
          ? "已暂停"
          : ago(last?.at);

    const tabButton = (key: "panel" | "settings", label: string) => (
      <Button
        onPress={() => tab.set(key)}
        style={{
          paddingHorizontal: 10,
          paddingVertical: 4,
          borderRadius: 8,
          backgroundColor: tc === key ? T.panel() : "transparent",
          borderWidth: 1,
          borderColor: tc === key ? T.stroke() : "transparent",
        }}
      >
        <Text style={{ fontSize: 12, fontWeight: tc === key ? "600" : "400", color: tc === key ? T.text() : T.text3() }}>
          {label}
        </Text>
      </Button>
    );

    const panelTab = () => (
      <View style={{ gap: 10 }}>
        <View style={{ flexDirection: "row", alignItems: "center", gap: 9 }}>
          <Dot color={dot} />
          <Text style={{ fontSize: 14, fontWeight: "600", color: T.text() }}>{d?.label ?? "读取中…"}</Text>
          <View style={{ flex: 1 }} />
          <Text style={{ fontSize: 11.5, color: T.text3() }}>{headline}</Text>
        </View>

        {upd ? (
          <Button
            onPress={doUpdate}
            disabled={isBusy}
            style={{
              paddingHorizontal: 10,
              paddingVertical: 7,
              borderRadius: 9,
              backgroundColor: token("brand-tint"),
              borderWidth: 1,
              borderColor: token("brand-edge"),
            }}
          >
            <Text style={{ fontSize: 12, color: T.text() }}>{"新版本 " + upd + " 可用 · 点此安装"}</Text>
          </Button>
        ) : null}

        <View style={{ flexDirection: "row", gap: 8 }}>
          <Stat label="采集" value={last ? String(last.captured) : "—"} unit="文件" />
          <Stat label="落盘" value={last ? String(last.applied_files) : "—"} unit="文件" />
          <Stat label="冲突" value={String(conflicts)} />
        </View>

        <Text style={{ fontSize: 10.5, color: T.text3() }}>设备</Text>
        <View style={{ gap: 2 }}>
          {peers.length === 0 ? (
            <Text style={{ fontSize: 11.5, color: T.text3(), paddingVertical: 6 }}>还没接入其它设备</Text>
          ) : null}
          {peers.map((p) => (
            <View style={{ flexDirection: "row", alignItems: "center", gap: 9, paddingVertical: 6, paddingHorizontal: 10 }}>
              <Dot color={p.last === "failed" ? T.danger() : p.last === "ok" ? T.ok() : T.text3()} />
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 13, fontWeight: "600", color: T.text() }}>{p.label || p.id}</Text>
                <Text style={{ fontSize: 11.5, color: T.text3() }}>{(p.lan || "无局域网地址") + " · " + p.ticket}</Text>
              </View>
              <Text style={{ fontSize: 10.5, color: p.last === "failed" ? T.danger() : T.text3() }}>
                {p.last === "failed" ? "拉取失败" : p.last === "ok" ? "已同步" : "待同步"}
              </Text>
            </View>
          ))}
        </View>

        <Text style={{ fontSize: 10.5, color: T.text3() }}>目录映射</Text>
        <View style={{ gap: 4 }}>
          {trees.map((t) => (
            <View
              style={{
                flexDirection: "row",
                alignItems: "center",
                gap: 8,
                paddingVertical: 6,
                paddingHorizontal: 10,
                borderRadius: 8,
                backgroundColor: T.panel(),
                borderWidth: 1,
                borderColor: T.stroke(),
              }}
            >
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, fontWeight: "600", color: T.text() }}>{t.name}</Text>
                <Text style={{ fontSize: 10.5, color: T.text3() }}>
                  {t.source + " · " + t.files + " 文件"}
                </Text>
              </View>
              <Toggle on={t.enabled} onChange={(v) => toggleTree(t.name, v)} />
            </View>
          ))}
        </View>

        <View style={{ flexDirection: "row", gap: 8, paddingTop: 8, borderTopWidth: 1, borderColor: T.stroke() }}>
          <Button
            onPress={() => act("sync")}
            disabled={!running || isBusy || syncing}
            style={{ paddingHorizontal: 12, paddingVertical: 7, borderRadius: 9, backgroundColor: T.accent() }}
          >
            <Text style={{ fontSize: 12.5, fontWeight: "600", color: T.onAccent() }}>
              {syncing ? "同步中…" : "立即同步"}
            </Text>
          </Button>
          <Button
            onPress={() => act(paused ? "resume" : "pause")}
            disabled={!running || isBusy}
            style={{
              paddingHorizontal: 12,
              paddingVertical: 7,
              borderRadius: 9,
              backgroundColor: T.panel2(),
              borderWidth: 1,
              borderColor: T.stroke(),
            }}
          >
            <Text style={{ fontSize: 12.5, color: T.text() }}>{paused ? "继续" : "暂停"}</Text>
          </Button>
          <View style={{ flex: 1 }} />
          <Button
            onPress={() => tab.set("settings")}
            style={{
              paddingHorizontal: 12,
              paddingVertical: 7,
              borderRadius: 9,
              backgroundColor: T.panel2(),
              borderWidth: 1,
              borderColor: T.stroke(),
            }}
          >
            <Text style={{ fontSize: 12.5, color: T.text() }}>设置…</Text>
          </Button>
        </View>
      </View>
    );

    const settingsTab = () => {
      const s = d?.sync;
      const u = d?.update_settings;
      return (
        <View style={{ gap: 8 }}>
          <Text style={{ fontSize: 10.5, color: T.text3() }}>同步</Text>
          <View style={{ borderRadius: 10, backgroundColor: T.panel(), borderWidth: 1, borderColor: T.stroke(), paddingHorizontal: 10 }}>
            <Stepper
              label="兜底轮询"
              hint="事件驱动为主，这里只补齐丢失的广播"
              value={Math.round((s?.tick_interval ?? 1800) / 60)}
              unit="分钟"
              onDelta={(dir) => setSync({ tick_interval: Math.max(1, Math.round((s?.tick_interval ?? 1800) / 60) + dir * 5) * 60 })}
            />
            <Stepper
              label="抖动窗口"
              hint="编辑器连续保存合并成一次同步"
              value={s?.watch_cooldown ?? 15}
              unit="秒"
              onDelta={(dir) => setSync({ watch_cooldown: Math.max(5, (s?.watch_cooldown ?? 15) + dir * 5) })}
            />
            <Stepper
              label="快照保留"
              hint="落盘前备份的份数"
              value={s?.backup_keep ?? 20}
              unit="份"
              onDelta={(dir) => setSync({ backup_keep: Math.max(1, Math.min(100, (s?.backup_keep ?? 20) + dir)) })}
            />
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingVertical: 8 }}>
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, color: s?.prune ? T.danger() : T.text() }}>删除也传播（镜像）</Text>
                <Text style={{ fontSize: 10.5, color: T.text3() }}>
                  关闭时只增不删；开启有误删风险，慎用
                </Text>
              </View>
              <Toggle on={s?.prune ?? false} onChange={(v) => setSync({ prune: v })} />
            </View>
          </View>

          <Text style={{ fontSize: 10.5, color: T.text3() }}>更新</Text>
          <View style={{ borderRadius: 10, backgroundColor: T.panel(), borderWidth: 1, borderColor: T.stroke(), paddingHorizontal: 10 }}>
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingVertical: 8 }}>
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, color: T.text() }}>自动更新</Text>
                <Text style={{ fontSize: 10.5, color: T.text3() }}>
                  每 {Math.round((u?.check_interval ?? 21600) / 3600)} 小时检查一次，有新版本自动安装
                </Text>
              </View>
              <Toggle on={u?.auto ?? true} onChange={(v) => setUpdate({ auto: v })} />
            </View>
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingVertical: 8 }}>
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, color: T.text() }}>立即检查</Text>
                <Text style={{ fontSize: 10.5, color: T.text3() }}>
                  当前 v{d?.version ?? "?"} · 发布通道 npm @jixoai/agent-sync
                </Text>
              </View>
              <Button
                onPress={checkUpdate}
                disabled={checking()}
                style={{ paddingHorizontal: 10, paddingVertical: 5, borderRadius: 8, backgroundColor: T.panel2(), borderWidth: 1, borderColor: T.stroke() }}
              >
                <Text style={{ fontSize: 12, color: T.text() }}>{checking() ? "检查中…" : "检查更新"}</Text>
              </Button>
            </View>
          </View>

          <Text style={{ fontSize: 10.5, color: T.text3() }}>配置文件</Text>
          <View style={{ borderRadius: 10, backgroundColor: T.panel(), borderWidth: 1, borderColor: T.stroke(), paddingHorizontal: 10 }}>
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingVertical: 8 }}>
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, color: T.text() }}>config.toml</Text>
                <Text style={{ fontSize: 10.5, color: T.text3() }}>目录映射等高级项在这里改（本机文件，不入库）</Text>
              </View>
              <Button
                onPress={() => copyPath(d?.config_path ?? "")}
                style={{ paddingHorizontal: 10, paddingVertical: 5, borderRadius: 8, backgroundColor: T.panel2(), borderWidth: 1, borderColor: T.stroke() }}
              >
                <Text style={{ fontSize: 12, color: T.text() }}>复制路径</Text>
              </Button>
              <Button
                onPress={openConfigFile}
                style={{ paddingHorizontal: 10, paddingVertical: 5, borderRadius: 8, backgroundColor: T.panel2(), borderWidth: 1, borderColor: T.stroke() }}
              >
                <Text style={{ fontSize: 12, color: T.text() }}>打开</Text>
              </Button>
            </View>
          </View>
        </View>
      );
    };

    return (
      <View style={{ flex: 1, padding: 14, gap: 8 }}>
        <View style={{ flexDirection: "row", alignItems: "center", gap: 6 }}>
          {tabButton("panel", "面板")}
          {tabButton("settings", "设置")}
          <View style={{ flex: 1 }} />
          {upd ? (
            <Text style={{ fontSize: 10.5, color: T.accent() }}>{"可更新 " + upd}</Text>
          ) : null}
        </View>

        {/* 固定高度：提示出现/消失时不让下面的内容跳动（点击会错位） */}
        <View style={{ height: 16, justifyContent: "center" }}>
          <Text style={{ fontSize: 11, color: T.accent() }}>{hint}</Text>
        </View>

        <View style={{ flex: 1 }}>{tc === "settings" ? settingsTab() : panelTab()}</View>

        <View style={{ flexDirection: "row", alignItems: "center", gap: 8, paddingTop: 6, borderTopWidth: 1, borderColor: T.stroke() }}>
          <Text style={{ fontSize: 11, color: e ? T.danger() : T.text3() }}>{e ? e : (d?.device ?? "…")}</Text>
          {!e ? (
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flex: 1 }}>
              <Text style={{ fontSize: 11, color: T.text3() }}>{"v" + (d?.version ?? "?")}</Text>
              <View style={{ flex: 1 }} />
              <Button onPress={checkUpdate} disabled={checking()} style={{ paddingVertical: 2, paddingHorizontal: 4 }}>
                <Text style={{ fontSize: 11, color: T.text3() }}>{checking() ? "检查中…" : "检查更新"}</Text>
              </Button>
              <Button onPress={() => Application.quit()} style={{ paddingVertical: 2, paddingHorizontal: 4 }}>
                <Text style={{ fontSize: 11, color: T.text3() }}>退出</Text>
              </Button>
            </View>
          ) : (
            <View style={{ flex: 1 }} />
          )}
        </View>
      </View>
    );
  };

  // solid-glass：玻璃材质由 NSGlassEffectView 绘制（macOS 26+），
  // 圆角、描边与阴影都交给主题的 finish，外层窗口保持透明。
  return (
    <View style={{ flex: 1, backgroundColor: "transparent" }}>
      <View
        style={[
          finish.glass,
          {
            flex: 1,
            borderRadius: token("radius-container"),
            borderWidth: 1,
            borderColor: T.stroke(),
            overflow: "hidden",
          },
        ]}
      >
        {content()}
      </View>
    </View>
  );
}

mount(App);
