import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  Check,
  CircleCheck,
  FolderOpen,
  HardDrive,
  Home,
  Info,
  Loader2,
  Lock,
  MessagesSquare,
  MonitorSmartphone,
  RotateCw,
  ShieldAlert,
} from "lucide-react";

import PekoMark from "../PekoMark";
import TitleBar from "../TitleBar";
import PrivacySwitches, { type Switches } from "../PrivacySwitches";
import { HelloStep, ModelStep, ONBOARDING_KEY, PekoStep } from "../FirstRunWalkthrough";
import {
  appSetupCheckFolder,
  appSetupFinish,
  appSetupUseFolder,
  engineStatus,
  pickFolder,
  principalList,
  privacySet,
  type AppSetupStatus,
  type FolderCheck,
} from "../../lib/api";
import { formatBytes } from "../../lib/format";
import { track } from "../../lib/usage";
import {
  LANGUAGES,
  getLanguageChoice,
  setLanguage,
  systemLanguage,
  type LanguageChoice,
} from "../../i18n";

const STEPS = ["welcome", "folder", "privacy", "ready", "model", "peko", "hello"] as const;
type Step = (typeof STEPS)[number];

/** How long "Start Peko" waits for the engine before showing the app anyway. */
const START_WAIT_MS = 20_000;

/** `/Users/ada/Peko` → `~/Peko`, for reading; the full path is in the title. */
function shortPath(path: string, defaultHome: string): string {
  const home = defaultHome.replace(/[\\/]\.peko$/, "");
  if (home && (path === home || path.startsWith(`${home}/`) || path.startsWith(`${home}\\`))) {
    return `~${path.slice(home.length)}`;
  }
  return path;
}

/**
 * The first-run setup (src-tauri/src/home.rs): a full-window wizard shown
 * before anything else when this computer has no Peko folder yet, or the
 * chosen one is gone. It chooses where Peko keeps things, asks about
 * crash reports and usage counts, and then starts the engine. The engine
 * waits for it: nothing is written to `~/.peko` before the owner chooses.
 *
 * Once the engine runs, the same window goes on to the first peko: connect
 * a model, name the peko, say hello (the steps `FirstRunWalkthrough` uses).
 * Any of those can be skipped; a folder that already has pekos skips them.
 */
export default function SetupWizard({
  status,
  onDone,
}: {
  status: AppSetupStatus;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const missing = status.reason === "missing";
  // A missing folder skips the welcome: this computer has seen Peko before.
  const [step, setStep] = useState<Step>(missing ? "folder" : "welcome");
  const [home, setHome] = useState<string | null>(null);
  const [privacy, setPrivacy] = useState<Switches>({ crashReports: true, usageEvents: true });
  const [modelId, setModelId] = useState<string | null>(null);
  const [pekoName, setPekoName] = useState<string | null>(null);
  const navigate = useNavigate();
  const index = STEPS.indexOf(step);
  const heading = useRef<HTMLHeadingElement>(null);

  // Each step's heading takes focus, so a screen reader reads where you are.
  useEffect(() => {
    heading.current?.focus();
  }, [step]);

  const go = (to: Step) => setStep(to);

  /** Setup is over: show the app, in the new peko's chat when there is one. */
  const finish = (how: "done" | "skipped" | "had pekos", name: string | null, say: string | null) => {
    // The overlay walkthrough covers the same steps: not again.
    try {
      localStorage.setItem(ONBOARDING_KEY, "1");
    } catch {
      // No storage: the overlay may offer the steps once more.
    }
    if (how !== "had pekos") track("onboarding_step", { step: how });
    onDone();
    if (name) {
      void navigate({ to: "/chat/$pekoName", params: { pekoName: name }, search: say ? { say } : {} });
    }
  };
  const skip = () => finish("skipped", pekoName, null);

  return (
    <div className="flex h-screen w-screen flex-col overflow-hidden bg-surface-rail" data-testid="setup-wizard">
      <TitleBar />
      <div className="flex flex-1 items-center justify-center overflow-auto p-6">
        <div className="flex h-[600px] w-full max-w-[920px] animate-scale-in overflow-hidden rounded-2xl border border-line bg-surface-card shadow-lifted">
          <Rail current={index} />
          <section className="flex min-w-0 flex-1 flex-col" aria-live="polite">
            <p className="eyebrow px-10 pt-8">
              {t("setup.stepOf", { current: index + 1, total: STEPS.length })}
            </p>
            {step === "welcome" && <Welcome headingRef={heading} onNext={() => go("folder")} />}
            {step === "folder" && (
              <FolderStep
                headingRef={heading}
                status={status}
                onBack={missing ? undefined : () => go("welcome")}
                onNext={(chosen) => {
                  setHome(chosen);
                  go("privacy");
                }}
              />
            )}
            {step === "privacy" && (
              <PrivacyStep
                headingRef={heading}
                value={privacy}
                onChange={setPrivacy}
                onBack={() => go("folder")}
                onNext={() => go("ready")}
              />
            )}
            {step === "ready" && (
              <ReadyStep
                headingRef={heading}
                status={status}
                home={home ?? status.home}
                privacy={privacy}
                onBack={() => go("privacy")}
                onStarted={(hasPekos) => (hasPekos ? finish("had pekos", null, null) : go("model"))}
              />
            )}
            {step === "model" && (
              <OnboardingFrame headingRef={heading} title={t("firstRun.steps.1")} onSkip={skip}>
                <ModelStep
                  onNext={(id) => {
                    setModelId(id);
                    track("onboarding_step", { step: "model" });
                    go("peko");
                  }}
                />
              </OnboardingFrame>
            )}
            {step === "peko" && modelId && (
              <OnboardingFrame headingRef={heading} title={t("firstRun.steps.2")} onSkip={skip}>
                <PekoStep
                  modelId={modelId}
                  takenNames={[]}
                  onBack={() => go("model")}
                  onNext={(name) => {
                    setPekoName(name);
                    track("onboarding_step", { step: "peko" });
                    go("hello");
                  }}
                />
              </OnboardingFrame>
            )}
            {step === "hello" && pekoName && (
              // The peko exists by now: its own "go to the chat" is the way out.
              <OnboardingFrame headingRef={heading} title={t("firstRun.steps.3")}>
                <HelloStep pekoName={pekoName} onFinish={(say) => finish("done", pekoName, say)} />
              </OnboardingFrame>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

// ─── The brand rail with the steps ───────────────────────────────────

function Rail({ current }: { current: number }) {
  const { t } = useTranslation();
  return (
    <aside className="relative hidden w-64 shrink-0 flex-col overflow-hidden bg-gradient-to-b from-peko-600 to-peko-800 p-7 text-white md:flex">
      {/* The sprout, large and faint, behind the steps. */}
      <PekoMark className="pointer-events-none absolute -bottom-10 -right-12 h-64 w-64 opacity-[0.12] [&_path]:!fill-white [&_path]:!stroke-white" />
      <div className="flex items-center gap-2.5">
        <span className="flex h-9 w-9 items-center justify-center rounded-xl bg-white/95 shadow-sm">
          <PekoMark className="h-6 w-6" />
        </span>
        <span className="text-lg font-semibold tracking-tight">Peko</span>
      </div>
      <ol className="relative mt-12 space-y-1" aria-label={t("setup.steps.label")}>
        {STEPS.map((key, i) => {
          const done = i < current;
          const active = i === current;
          return (
            <li
              key={key}
              aria-current={active ? "step" : undefined}
              className={[
                "flex items-center gap-3 rounded-lg px-2 py-2 text-sm transition-colors",
                active ? "bg-white/15 font-medium text-white" : done ? "text-white/85" : "text-white/55",
              ].join(" ")}
            >
              <span
                className={[
                  "flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-xs font-semibold transition-colors",
                  done
                    ? "bg-lime text-lime-fg"
                    : active
                      ? "bg-white text-peko-700"
                      : "border border-white/40",
                ].join(" ")}
              >
                {done ? <Check className="h-3.5 w-3.5" strokeWidth={3} /> : i + 1}
              </span>
              {t(`setup.steps.${key}`)}
            </li>
          );
        })}
      </ol>
    </aside>
  );
}

// ─── Shared step frame ───────────────────────────────────────────────

function StepFrame({
  title,
  body,
  headingRef,
  children,
  footer,
}: {
  title: string;
  body?: React.ReactNode;
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  children?: React.ReactNode;
  footer: React.ReactNode;
}) {
  return (
    <>
      <div className="min-h-0 flex-1 animate-fade-up overflow-y-auto px-10 pb-6 pt-2">
        {/* Focused for screen readers on each step, not for show. */}
        <h1
          ref={headingRef}
          tabIndex={-1}
          style={{ outline: "none" }}
          className="text-2xl font-semibold tracking-tight text-fg"
        >
          {title}
        </h1>
        {body && <div className="mt-2 max-w-prose text-sm leading-relaxed text-fg-muted">{body}</div>}
        {children && <div className="mt-6">{children}</div>}
      </div>
      {footer && (
        <div className="flex items-center justify-between gap-3 border-t border-line px-10 py-4">{footer}</div>
      )}
    </>
  );
}

function BackButton({ onBack, disabled }: { onBack?: () => void; disabled?: boolean }) {
  const { t } = useTranslation();
  if (!onBack) return <span />;
  return (
    <button type="button" className="btn-ghost" onClick={onBack} disabled={disabled}>
      {t("setup.back")}
    </button>
  );
}

// ─── 1. Welcome ──────────────────────────────────────────────────────

function Welcome({
  headingRef,
  onNext,
}: {
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  onNext: () => void;
}) {
  const { t } = useTranslation();
  const [language, setLanguageChoice] = useState<LanguageChoice>(getLanguageChoice());
  const systemName = LANGUAGES.find((l) => l.code === systemLanguage())?.name ?? "English";
  const points = [
    { icon: MonitorSmartphone, title: "local", hint: "localHint" },
    { icon: Lock, title: "keys", hint: "keysHint" },
    { icon: MessagesSquare, title: "together", hint: "togetherHint" },
  ] as const;

  return (
    <StepFrame
      headingRef={headingRef}
      title={t("setup.welcome.title")}
      body={
        <>
          <span className="font-medium text-brand-text">{t("setup.welcome.slogan")}</span>{" "}
          {t("setup.welcome.body")}
        </>
      }
      footer={
        <>
          <label className="flex items-center gap-2 text-sm text-fg-muted">
            <span>{t("setup.welcome.language")}</span>
            <select
              className="select input-sm w-48"
              value={language}
              data-testid="setup-language"
              onChange={(e) => {
                const choice = e.target.value as LanguageChoice;
                setLanguageChoice(choice);
                void setLanguage(choice);
              }}
            >
              <option value="system">{t("setup.welcome.system", { name: systemName })}</option>
              {LANGUAGES.map((l) => (
                <option key={l.code} value={l.code}>
                  {l.name}
                </option>
              ))}
            </select>
          </label>
          <button type="button" className="btn-primary px-5" onClick={onNext} data-testid="setup-start" autoFocus>
            {t("setup.welcome.start")}
          </button>
        </>
      }
    >
      <ul className="flex flex-col gap-3">
        {points.map(({ icon: Icon, title, hint }) => (
          <li key={title} className="card flex items-start gap-3.5 p-4">
            <span className="tile h-9 w-9">
              <Icon className="h-4.5 w-4.5" />
            </span>
            <span>
              <span className="block text-sm font-medium text-fg">{t(`setup.welcome.${title}`)}</span>
              <span className="field-hint mt-0.5 block">{t(`setup.welcome.${hint}`)}</span>
            </span>
          </li>
        ))}
      </ul>
    </StepFrame>
  );
}

// ─── 2. Folder ───────────────────────────────────────────────────────

function FolderStep({
  headingRef,
  status,
  onBack,
  onNext,
}: {
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  status: AppSetupStatus;
  onBack?: () => void;
  onNext: (home: string) => void;
}) {
  const { t } = useTranslation();
  const missing = status.reason === "missing";
  const [choice, setChoice] = useState<"default" | "other">(missing ? "other" : "default");
  const [picked, setPicked] = useState<string | null>(missing ? status.home : null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const defaultCheck = useQuery({
    queryKey: ["setup-folder", status.defaultHome],
    queryFn: () => appSetupCheckFolder(status.defaultHome),
  });
  const otherCheck = useQuery({
    queryKey: ["setup-folder", picked],
    queryFn: () => appSetupCheckFolder(picked as string),
    enabled: !!picked,
  });

  const check = choice === "default" ? defaultCheck.data : picked ? otherCheck.data : undefined;
  // A missing folder that is still missing would be made afresh: that is a
  // new, empty Peko, not the one the owner had. Only "Try again" or another
  // folder moves on.
  const stillMissing =
    missing && choice === "other" && picked === status.home && (!check || check.kind === "new");
  const canContinue = !!check && !check.problem && !saving && !stillMissing;

  const choose = async () => {
    const path = await pickFolder(picked ?? undefined);
    if (path) {
      setPicked(path);
      setChoice("other");
      setError(null);
    }
  };

  const save = async () => {
    if (!check) return;
    setSaving(true);
    setError(null);
    try {
      const home = await appSetupUseFolder(choice === "default" ? null : check.home);
      onNext(home);
    } catch (e) {
      const message = String(e instanceof Error ? e.message : e);
      const known = ["notAbsolute", "notFolder", "notWritable", "tooLong", "system", "insideApp"];
      setError(known.includes(message) ? t(`setup.folder.problem.${message as "system"}`) : message);
    } finally {
      setSaving(false);
    }
  };

  return (
    <StepFrame
      headingRef={headingRef}
      title={t(missing ? "setup.folder.missingTitle" : "setup.folder.title")}
      body={
        missing
          ? t("setup.folder.missingBody", { path: status.home })
          : t("setup.folder.body")
      }
      footer={
        <>
          <BackButton onBack={onBack} disabled={saving} />
          <button
            type="button"
            className="btn-primary px-5"
            disabled={!canContinue}
            onClick={() => void save()}
            data-testid="setup-folder-continue"
          >
            {saving && <Loader2 className="h-4 w-4 animate-spin" />}
            {saving ? t("setup.folder.saving") : t("setup.continue")}
          </button>
        </>
      }
    >
      {missing && (
        <div className="mb-4 flex items-center gap-3">
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={() => {
              setChoice("other");
              setPicked(status.home);
              void otherCheck.refetch();
            }}
            data-testid="setup-retry"
          >
            <RotateCw className={["h-3.5 w-3.5", otherCheck.isFetching ? "animate-spin" : ""].join(" ")} />
            {t("setup.folder.retry")}
          </button>
        </div>
      )}
      <div role="radiogroup" className="flex flex-col gap-3">
        <FolderOption
          selected={choice === "default"}
          onSelect={() => setChoice("default")}
          icon={Home}
          title={t("setup.folder.home")}
          badge={t("setup.folder.recommended")}
          hint={t("setup.folder.homeHint")}
          path={shortPath(status.defaultHome, status.defaultHome)}
          fullPath={status.defaultHome}
          check={defaultCheck.data}
          testid="setup-folder-default"
        />
        <FolderOption
          selected={choice === "other"}
          onSelect={() => (picked ? setChoice("other") : void choose())}
          icon={HardDrive}
          title={t("setup.folder.other")}
          hint={t("setup.folder.otherHint")}
          path={otherCheck.data ? shortPath(otherCheck.data.home, status.defaultHome) : null}
          fullPath={otherCheck.data?.home ?? null}
          check={otherCheck.data}
          action={
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={(e) => {
                e.stopPropagation();
                void choose();
              }}
              data-testid="setup-folder-choose"
            >
              <FolderOpen className="h-3.5 w-3.5" />
              {picked ? t("setup.folder.change") : t("setup.folder.choose")}
            </button>
          }
          testid="setup-folder-other"
        />
      </div>
      {stillMissing ? (
        <p className="note-warn mt-4 text-[13px]" data-testid="setup-still-missing">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          {t("setup.folder.stillMissing")}
        </p>
      ) : (
        check && <FolderNotes check={check} picked={choice === "other" ? picked : null} />
      )}
      {error && (
        <p className="note-danger mt-4" role="alert">
          <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0" />
          {error}
        </p>
      )}
    </StepFrame>
  );
}

function FolderOption({
  selected,
  onSelect,
  icon: Icon,
  title,
  badge,
  hint,
  path,
  fullPath,
  check,
  action,
  testid,
}: {
  selected: boolean;
  onSelect: () => void;
  icon: typeof Home;
  title: string;
  badge?: string;
  hint: string;
  path: string | null;
  fullPath: string | null;
  check: FolderCheck | undefined;
  action?: React.ReactNode;
  testid: string;
}) {
  const { t } = useTranslation();
  return (
    <div
      role="radio"
      aria-checked={selected}
      tabIndex={0}
      onClick={onSelect}
      onKeyDown={(e) => {
        if (e.key === " " || e.key === "Enter") {
          e.preventDefault();
          onSelect();
        }
      }}
      data-testid={testid}
      className={[
        "flex min-w-0 cursor-pointer items-start gap-3.5 rounded-xl border p-4 outline-none transition-all duration-150 focus-visible:ring-2 focus-visible:ring-accent-soft",
        selected
          ? "border-brand bg-brand-soft/60 shadow-sm ring-1 ring-brand/30"
          : "border-line bg-surface-card hover:border-line-strong hover:bg-surface-sunken/60",
      ].join(" ")}
    >
      <span
        className={[
          "mt-0.5 flex h-4.5 w-4.5 shrink-0 items-center justify-center rounded-full border-2 transition-colors",
          selected ? "border-brand bg-brand" : "border-line-strong",
        ].join(" ")}
        aria-hidden
      >
        {selected && <span className="h-1.5 w-1.5 rounded-full bg-brand-fg" />}
      </span>
      <span className="tile h-9 w-9">
        <Icon className="h-4.5 w-4.5" />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2">
          <span className="text-sm font-medium text-fg">{title}</span>
          {badge && <span className="badge-accent">{badge}</span>}
        </span>
        <span className="field-hint mt-0.5 block">{hint}</span>
        {path && (
          <span className="mt-2 flex min-w-0 items-center gap-2">
            <code className="code-inline min-w-0 truncate" title={fullPath ?? undefined} dir="ltr">
              {path}
            </code>
            {check?.freeBytes != null && (
              <span className="badge-neutral shrink-0">
                {t("setup.folder.free", { size: formatBytes(check.freeBytes) })}
              </span>
            )}
          </span>
        )}
      </span>
      {action && <span className="shrink-0 self-center">{action}</span>}
    </div>
  );
}

/** What choosing the folder does, and anything to know first. */
function FolderNotes({ check, picked }: { check: FolderCheck; picked: string | null }) {
  const { t } = useTranslation();
  const notes: { tone: "ok" | "info" | "warn" | "danger"; text: string; key: string }[] = [];
  if (check.problem) {
    notes.push({ tone: "danger", key: "problem", text: t(`setup.folder.problem.${check.problem}`) });
  } else {
    const inside = !!picked && check.home !== picked && check.kind === "new";
    const pekos = check.pekos ?? 0;
    // `~/.peko` needs no saying, unless it already holds pekos.
    const kindText =
      check.kind === "peko" || (check.kind === "default" && pekos > 0)
        ? pekos > 0
          ? t("setup.folder.kind.peko", { count: pekos })
          : t("setup.folder.kind.pekoEmpty")
        : check.kind === "default"
          ? null
          : inside
            ? t("setup.folder.kind.subfolder")
            : t(`setup.folder.kind.${check.kind}`);
    if (kindText) {
      notes.push({ tone: check.kind === "new" || check.kind === "empty" ? "info" : "ok", key: "kind", text: kindText });
    }
    for (const w of check.warnings) {
      notes.push({ tone: w === "synced" ? "warn" : "info", key: w, text: t(`setup.folder.warning.${w}`) });
    }
  }
  const style = {
    ok: "note border-ok/25 bg-ok-soft text-ok",
    info: "note-info",
    warn: "note-warn",
    danger: "note-danger",
  } as const;
  const icon = { ok: CircleCheck, info: Info, warn: AlertTriangle, danger: ShieldAlert } as const;
  return (
    <ul className="mt-4 space-y-2" data-testid="setup-folder-notes">
      {notes.map((n) => {
        const Icon = icon[n.tone];
        return (
          <li key={n.key} className={`${style[n.tone]} text-[13px]`}>
            <Icon className="mt-0.5 h-4 w-4 shrink-0" />
            <span>{n.text}</span>
          </li>
        );
      })}
    </ul>
  );
}

// ─── 3. Privacy ──────────────────────────────────────────────────────

function PrivacyStep({
  headingRef,
  value,
  onChange,
  onBack,
  onNext,
}: {
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  value: Switches;
  onChange: (next: Switches) => void;
  onBack: () => void;
  onNext: () => void;
}) {
  const { t } = useTranslation();
  return (
    <StepFrame
      headingRef={headingRef}
      title={t("setup.privacy.title")}
      body={t("settings.privacy.noticeBody")}
      footer={
        <>
          <BackButton onBack={onBack} />
          <button type="button" className="btn-primary px-5" onClick={onNext} data-testid="setup-privacy-continue">
            {t("setup.continue")}
          </button>
        </>
      }
    >
      <div className="card p-5">
        <PrivacySwitches value={value} onChange={onChange} />
      </div>
      <p className="field-hint mt-3">{t("settings.privacy.changeLater")}</p>
    </StepFrame>
  );
}

// ─── 4. Ready ────────────────────────────────────────────────────────

function ReadyStep({
  headingRef,
  status,
  home,
  privacy,
  onBack,
  onStarted,
}: {
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  status: AppSetupStatus;
  home: string;
  privacy: Switches;
  onBack: () => void;
  onStarted: (hasPekos: boolean) => void;
}) {
  const { t } = useTranslation();
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const onOff = (on: boolean) => (on ? t("setup.ready.on") : t("setup.ready.off"));

  const start = async () => {
    setStarting(true);
    setError(null);
    try {
      // Saved in the chosen folder, so only once it exists.
      await privacySet({ ...privacy, acknowledged: true });
      await appSetupFinish();
      // Go on once the engine is up, or after a while regardless: the next
      // steps wait for it themselves, and the app has its own word for an
      // engine that is slow or failed.
      const deadline = Date.now() + START_WAIT_MS;
      while (Date.now() < deadline) {
        const state = await engineStatus().catch(() => null);
        if (state?.kind === "running") break;
        if (state?.kind === "failed") throw new Error(state.message);
        await new Promise((r) => setTimeout(r, 400));
      }
      // A folder that already has pekos needs no first peko.
      const pekos = await principalList().catch(() => []);
      onStarted(pekos.length > 0);
    } catch (e) {
      setError(t("setup.ready.failed", { message: String(e instanceof Error ? e.message : e) }));
      setStarting(false);
    }
  };

  const rows = [
    { key: "folder", label: t("setup.ready.folder"), value: shortPath(home, status.defaultHome), title: home },
    { key: "crash", label: t("setup.ready.crash"), value: onOff(privacy.crashReports) },
    { key: "usage", label: t("setup.ready.usage"), value: onOff(privacy.usageEvents) },
  ];

  return (
    <StepFrame
      headingRef={headingRef}
      title={t("setup.ready.title")}
      body={t("setup.ready.body")}
      footer={
        <>
          <BackButton onBack={onBack} disabled={starting} />
          <button
            type="button"
            className="btn-primary px-5"
            onClick={() => void start()}
            disabled={starting}
            data-testid="setup-finish"
          >
            {starting && <Loader2 className="h-4 w-4 animate-spin" />}
            {starting ? t("setup.ready.starting") : t("setup.ready.start")}
          </button>
        </>
      }
    >
      <dl className="rows">
        {rows.map((r) => (
          <div key={r.key} className="flex items-center justify-between gap-4 px-4 py-3">
            <dt className="text-sm text-fg-muted">{r.label}</dt>
            <dd
              className={["min-w-0 truncate text-fg-soft", r.title ? "font-mono text-xs" : "text-sm"].join(" ")}
              title={r.title}
            >
              {r.value}
            </dd>
          </div>
        ))}
      </dl>
      <h2 className="eyebrow mt-6">{t("setup.ready.tips")}</h2>
      <ul className="mt-2 space-y-2 text-sm text-fg-muted">
        <li className="flex items-start gap-2.5">
          <Check className="mt-0.5 h-4 w-4 shrink-0 text-brand-text" />
          {t(`setup.ready.tray.${status.platform}`)}
        </li>
        <li className="flex items-start gap-2.5">
          <Check className="mt-0.5 h-4 w-4 shrink-0 text-brand-text" />
          {t("setup.ready.terminal")}
        </li>
      </ul>
      {error && (
        <p className="note-danger mt-4" role="alert">
          <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0" />
          {error}
        </p>
      )}
    </StepFrame>
  );
}

// ─── 5–7. The first peko, skippable ──────────────────────────────────

/**
 * A first-peko step: the walkthrough's own component (with its own
 * buttons) in the wizard's frame, and a way out to the app.
 */
function OnboardingFrame({
  headingRef,
  title,
  onSkip,
  children,
}: {
  headingRef: React.RefObject<HTMLHeadingElement | null>;
  title: string;
  /** Leave for the app; absent where the step has its own way out. */
  onSkip?: () => void;
  children: React.ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <StepFrame
      headingRef={headingRef}
      title={title}
      footer={
        onSkip ? (
          <>
            <span className="field-hint">{t("setup.skipHint")}</span>
            <button type="button" className="btn-ghost" onClick={onSkip} data-testid="setup-skip">
              {t("setup.skip")}
            </button>
          </>
        ) : null
      }
    >
      {children}
    </StepFrame>
  );
}
