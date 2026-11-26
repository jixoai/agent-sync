// agents-sync 菜单栏面板。
//
// 数据全部来自 Rust（`asui/native`）：实时运行态走 daemon 的 unix socket，
// 设备清单走 devices.json，目录映射与开关走 config.toml / config.local.toml。
// TSX 只负责渲染与发动作，不发明状态——面板上每个数字都能在命令行复现。
//
// nsh 的反应式规则（踩坑后）：**组件体只跑一次**，信号必须在 JSX 表达式里读。
// 所以视觉树放在 `content()`，由 `{content()}` 调用；信号变化时它整体重跑。
//
// 令牌与设计稿一致（/tmp/shots/popover-{light,dark}.png）。
import { mount, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import { Sync } from "asui/native";

const T = {
  bg: "#eef1f8",
  panel: "#ffffff",
  panel2: "#f6f7fb",
  stroke: "rgba(0,0,0,0.08)",
  text: "#17171a",
  text2: "#4a4a52",
  text3: "#8a8a94",
  accent: "#0a6cff",
  ok: "#1fa94f",
  warn: "#d98a00",
  danger: "#d93025",
} as const;

type Peer = {
  id: string;
  label: string;
  lan: string;
  ticket: string;
  last: "ok" | "failed" | "unknown";
};

type Tree = {
  name: string;
  source: string;
  payload: string;
  target: string;
  enabled: boolean;
  files: number;
};

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
  peers: Peer[];
  trees: Tree[];
};

function Dot(props: { color: string }) {
  return (
    <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: props.color }} />
  );
}

function Stat(props: { label: string; value: string; unit?: string }) {
  return (
    <View
      style={{
        flex: 1,
        padding: 10,
        borderRadius: 10,
        backgroundColor: T.panel2,
        borderWidth: 1,
        borderColor: T.stroke,
      }}
    >
      <Text style={{ fontSize: 10.5, color: T.text3 }}>{props.label}</Text>
      <View style={{ flexDirection: "row", alignItems: "flex-end", gap: 3 }}>
        <Text style={{ fontSize: 19, fontWeight: "600", color: T.text }}>{props.value}</Text>
        {props.unit ? <Text style={{ fontSize: 11, color: T.text3 }}>{props.unit}</Text> : null}
      </View>
    </View>
  );
}

/** 相对时间：面板每 4 秒刷新一次，显示「12s 前」比绝对时间更有用。 */
function ago(iso: string | undefined): string {
  if (!iso) return "尚未同步";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return iso;
  const s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return `${s}s 前`;
  if (s < 3600) return `${Math.round(s / 60)} 分钟前`;
  return `${Math.round(s / 3600)} 小时前`;
}

/** 设计稿里的胶囊开关。用 Button 承载按压/键盘语义，外观自己画。 */
function Toggle(props: { on: boolean; onChange: (v: boolean) => void }) {
  return (
    <Button
      onPress={() => props.onChange(!props.on)}
      accessibilityLabel={props.on ? "暂停此目录" : "启用此目录"}
      style={{
        width: 30,
        height: 18,
        borderRadius: 999,
        backgroundColor: props.on ? T.accent : "rgba(138,138,148,0.35)",
        justifyContent: "center",
        padding: 2,
        // 开启时拇指引到右端（关闭时靠左）
        alignItems: props.on ? "flex-end" : "flex-start",
      }}
    >
      <View style={{ width: 14, height: 14, borderRadius: 7, backgroundColor: "#ffffff" }} />
    </Button>
  );
}

function App() {
  const ov = signal<Overview | null>(null);
  const err = signal<string>("");
  const busy = signal(false);

  const load = () => {
    try {
      const text = Sync.overview();
      const next = JSON.parse(text) as Overview;
      ov.set(next);
      if (!next.ok) err.set(next.error ?? "读取失败");
    } catch (e) {
      err.set(String(e));
    }
  };

  // 首读 + 每 4 秒刷新。
  //
  // 用普通定时器而不是 effect()：实测在 nsh 0.6.1 的 JS 运行时里
  // `effect` 里的回调没有执行（面板一直停在「读取中…」），
  // 而定时器只依赖全局对象，语义最稳。组件体只跑一次，正好用来装轮询。
  setTimeout(load, 0);
  setInterval(load, 4000);

  const act = (cmd: string) => {
    busy.set(true);
    try {
      const r = JSON.parse(Sync.action(cmd)) as { ok: boolean; error?: string };
      if (!r.ok && r.error) err.set(r.error);
      else err.set("");
    } catch (e) {
      err.set(String(e));
    }
    load();
    setTimeout(() => {
      busy.set(false);
      load();
    }, 600);
  };

  const toggleTree = (name: string, enabled: boolean) => {
    try {
      const r = JSON.parse(Sync.set_tree_enabled(name, enabled)) as { ok: boolean; error?: string };
      if (!r.ok && r.error) err.set(r.error);
      else err.set("");
    } catch (e) {
      err(String(e));
    }
    load();
  };

  const openConfig = () => {
    try {
      const r = JSON.parse(Sync.open_config()) as { ok: boolean; error?: string };
      if (!r.ok && r.error) err.set(r.error);
      else err.set("");
    } catch (e) {
      err.set(String(e));
    }
  };

  // 自更新：CLI 负责校验与替换，UI 只按钮 + 结果显示。
  const doUpdate = () => {
    busy.set(true);
    try {
      const r = JSON.parse(Sync.update()) as { ok: boolean; error?: string; output?: string };
      if (!r.ok) err.set(r.error || "更新失败");
      else err.set("");
    } catch (e) {
      err.set(String(e));
    }
    load();
    setTimeout(() => {
      busy.set(false);
      load();
    }, 800);
  };

  // ⚠️ 这里读信号（不是组件体）：nsh 只在这个表达式的求值里建立依赖。
  const content = () => {
    const d = ov();
    const e = err();
    const isBusy = busy();

    const running = d?.daemon.running ?? false;
    const paused = d?.daemon.paused ?? false;
    const syncing = d?.daemon.syncing ?? false;
    const last = d?.last ?? null;
    const conflicts = last?.conflicts?.length ?? 0;
    const peers = d?.peers ?? [];
    const trees = d?.trees ?? [];

    const dot = !running ? T.text3 : conflicts > 0 ? T.danger : syncing ? T.warn : T.ok;
    const headline = !running
      ? e
        ? "daemon 未启动"
        : "连接中…"
      : syncing
        ? "同步中…"
        : paused
          ? "已暂停"
          : ago(last?.at);

    return (
      <View style={{ flex: 1, padding: 16, gap: 10 }}>
        {/* 状态行 */}
        <View style={{ flexDirection: "row", alignItems: "center", gap: 9 }}>
          <Dot color={dot} />
          <Text style={{ fontSize: 14, fontWeight: "600", color: T.text }}>
            {d?.label ?? "读取中…"}
          </Text>
          <View style={{ flex: 1 }} />
          {d?.update_available ? (
            <Button
              onPress={doUpdate}
              disabled={isBusy}
              style={{
                paddingHorizontal: 8,
                paddingVertical: 3,
                borderRadius: 999,
                backgroundColor: "rgba(10,108,255,0.12)",
                borderWidth: 1,
                borderColor: "rgba(10,108,255,0.30)",
              }}
            >
              <Text style={{ fontSize: 10.5, color: T.accent }}>
                {"更新到 " + d.update_available}
              </Text>
            </Button>
          ) : (
            <Text style={{ fontSize: 11.5, color: T.text3 }}>{headline}</Text>
          )}
        </View>

        {/* 概览 */}
        <View style={{ flexDirection: "row", gap: 8 }}>
          <Stat label="采集" value={last ? String(last.captured) : "—"} unit="文件" />
          <Stat label="落盘" value={last ? String(last.applied_files) : "—"} unit="文件" />
          <Stat label="冲突" value={String(conflicts)} />
        </View>

        {/* 设备 */}
        <Text style={{ fontSize: 10.5, color: T.text3 }}>设备</Text>
        <View style={{ gap: 2 }}>
          {peers.length === 0 ? (
            <Text style={{ fontSize: 11.5, color: T.text3, paddingVertical: 6 }}>
              devices.json 里还没有对端
            </Text>
          ) : null}
          {peers.map((p) => (
            <View
              style={{
                flexDirection: "row",
                alignItems: "center",
                gap: 9,
                paddingVertical: 7,
                paddingHorizontal: 10,
              }}
            >
              <Dot color={p.last === "failed" ? T.danger : p.last === "ok" ? T.ok : T.text3} />
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 13, fontWeight: "600", color: T.text }}>
                  {p.label || p.id}
                </Text>
                <Text style={{ fontSize: 11.5, color: T.text3 }}>
                  {(p.lan || "无局域网地址") + " · " + p.ticket}
                </Text>
              </View>
              <View
                style={{
                  paddingHorizontal: 7,
                  paddingVertical: 2,
                  borderRadius: 999,
                  borderWidth: 1,
                  borderColor:
                    p.last === "failed" ? "rgba(217,48,37,0.28)" : "rgba(10,108,255,0.22)",
                  backgroundColor:
                    p.last === "failed" ? "rgba(217,48,37,0.10)" : "rgba(10,108,255,0.10)",
                }}
              >
                <Text style={{ fontSize: 10.5, color: p.last === "failed" ? T.danger : T.accent }}>
                  {p.last === "failed" ? "拉取失败" : p.last === "ok" ? "已同步" : "待同步"}
                </Text>
              </View>
            </View>
          ))}
        </View>

        {/* 目录映射（配置的可视化入口） */}
        <Text style={{ fontSize: 10.5, color: T.text3 }}>目录映射</Text>
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
                backgroundColor: T.panel,
                borderWidth: 1,
                borderColor: T.stroke,
              }}
            >
              <View style={{ flex: 1 }}>
                <Text style={{ fontSize: 12, fontWeight: "600", color: T.text }}>
                  {t.name}
                  <Text style={{ fontSize: 10.5, fontWeight: "400", color: T.text3 }}>
                    {"  " + t.files + " 文件"}
                  </Text>
                </Text>
                <Text style={{ fontSize: 10.5, color: T.text3 }}>
                  {t.source + " → payload/" + t.payload}
                </Text>
              </View>
              <Toggle on={t.enabled} onChange={(v) => toggleTree(t.name, v)} />
            </View>
          ))}
        </View>

        {/* 动作 */}
        <View
          style={{
            flexDirection: "row",
            gap: 8,
            paddingTop: 10,
            borderTopWidth: 1,
            borderColor: T.stroke,
          }}
        >
          <Button
            onPress={() => act("sync")}
            disabled={!running || isBusy || syncing}
            style={{
              paddingHorizontal: 12,
              paddingVertical: 7,
              borderRadius: 9,
              backgroundColor: T.accent,
            }}
          >
            <Text style={{ fontSize: 12.5, fontWeight: "600", color: "#ffffff" }}>
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
              backgroundColor: T.panel2,
              borderWidth: 1,
              borderColor: T.stroke,
            }}
          >
            <Text style={{ fontSize: 12.5, color: T.text }}>{paused ? "继续" : "暂停"}</Text>
          </Button>
          <View style={{ flex: 1 }} />
          <Button
            onPress={openConfig}
            style={{
              paddingHorizontal: 12,
              paddingVertical: 7,
              borderRadius: 9,
              backgroundColor: T.panel2,
              borderWidth: 1,
              borderColor: T.stroke,
            }}
          >
            <Text style={{ fontSize: 12.5, color: T.text }}>配置…</Text>
          </Button>
        </View>

        {/* 页脚：出错时优先显示错误，否则显示身份与版本 */}
        <View style={{ flexDirection: "row", alignItems: "center", gap: 8 }}>
          <Text style={{ fontSize: 11, color: e ? T.danger : T.text3 }}>
            {e ? e : (d?.device ?? "…")}
          </Text>
          {!e ? (
            <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flex: 1 }}>
              <Text style={{ fontSize: 11, color: T.text3 }}>{"v" + (d?.version ?? "?")}</Text>
              <Text style={{ fontSize: 11, color: T.text3 }}>·</Text>
              <Text style={{ fontSize: 11, color: T.text3 }}>{d?.jj_version ?? "…"}</Text>
              <View style={{ flex: 1 }} />
              <Button
                onPress={() => act("quit")}
                style={{ paddingVertical: 2, paddingHorizontal: 4 }}
              >
                <Text style={{ fontSize: 11, color: T.text3 }}>退出</Text>
              </Button>
            </View>
          ) : (
            <View style={{ flex: 1 }} />
          )}
        </View>
      </View>
    );
  };

  return <View style={{ flex: 1, backgroundColor: T.bg }}>{content()}</View>;
}

mount(App);
