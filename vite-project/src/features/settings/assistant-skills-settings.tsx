import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import {
    AlertDialog, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from '@/components/ui/alert-dialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Disclosure } from '@/components/ui/disclosure';
import { Switch } from '@/components/ui/switch';
import { listAiAssistantSkillProposals, listBundledSkills, setBundledSkill } from '@/services/clients';
import type {
    BundledSkillList, BundledSkillView, SkillDetail, SkillProposalView, SkillSummary, SkillVersionInfo, SkillVersionView,
} from '@/services/types';
import { SkillProposalCards } from '@/features/desk/assistant-skill-proposal-card';
import { isSkillConflict, skillApi, skillErrorText, type SkillApi, type SkillScopeKind } from './assistant-skills-api';
import { AssistantSkillEditor } from './assistant-skill-editor';
import { AssistantSkillImportDialog } from './assistant-skill-import-dialog';
import { ScanReport, scanNeedsConfirmation, SkillDiffList, SkillFileList } from './assistant-skill-review';

const prefix = 'pages.assistantSkills.';

type Failure = { text: string; conflict: boolean };
type Overlay =
    | { kind: 'detail'; skillId: string }
    | { kind: 'bundled'; name: string }
    | { kind: 'import'; base?: SkillSummary }
    | { kind: 'edit'; detail: SkillDetail };

async function loadPendingProposals(): Promise<SkillProposalView[]> {
    const body = await listAiAssistantSkillProposals();
    if (!body.success || !body.data) throw new Error('Skill proposals unavailable');
    return body.data.filter(proposal => proposal.status === 'pending');
}

async function loadBundled(): Promise<BundledSkillList> {
    const body = await listBundledSkills();
    if (!body.success || !body.data) throw new Error('Bundled skills unavailable');
    return body.data;
}

function SkillBadges({ skill }: { skill: SkillSummary }) {
    const { t } = useTranslation();
    return <>
        <Badge variant="secondary">{t(`${prefix}origin.${skill.origin}`)}</Badge>
        {skill.decisionSource && <Badge variant="outline">{t(`${prefix}decision.${skill.decisionSource}`)}</Badge>}
    </>;
}

function SkillRow({ name, description, badges, enabled, busy, onToggle, onOpen }: {
    name: string; description: string; badges: ReactNode; enabled: boolean; busy: boolean;
    onToggle(enabled: boolean): void; onOpen(): void;
}) {
    const { t } = useTranslation();
    return <li className="flex items-start gap-3 py-3">
        <div className="min-w-0 flex-1 space-y-1">
            <div className="flex flex-wrap items-center gap-2">
                <Button variant="link" className="h-auto p-0 font-mono text-sm font-medium" onClick={onOpen}>{name}</Button>
                {badges}
            </div>
            <p className="line-clamp-2 text-sm text-muted-foreground">{description}</p>
        </div>
        <Switch aria-label={t(`${prefix}enable`, { name })} checked={enabled} disabled={busy} onCheckedChange={onToggle} />
    </li>;
}

function SkillVersionHistory({ api, skillId, current, versions }: {
    api: SkillApi; skillId: string; current: number; versions: SkillVersionInfo[];
}) {
    const { t, i18n } = useTranslation();
    const [selected, setSelected] = useState<SkillVersionView | null>(null);
    const [loading, setLoading] = useState<number | null>(null);
    const [error, setError] = useState('');
    const open = async (version: number) => {
        if (selected?.info.version === version) { setSelected(null); return; }
        setLoading(version); setError('');
        try { setSelected(await api.version(skillId, version)); }
        catch (cause) { setSelected(null); setError(skillErrorText(t, cause)); }
        finally { setLoading(null); }
    };
    const number = (value: number) => new Intl.NumberFormat(i18n.language).format(value);
    return <section className="space-y-2">
        <p className="text-sm font-medium">{t(`${prefix}versions`)}</p>
        <ul className="divide-y rounded-md border text-sm">
            {versions.map(version => <li key={version.version} className="flex flex-wrap items-center gap-2 px-3 py-1.5">
                {version.version === current
                    ? <span className="font-medium">{t(`${prefix}versionValue`, { version: version.version })}</span>
                    : <Button variant="link" className="h-auto p-0 font-medium" disabled={loading != null}
                        aria-pressed={selected?.info.version === version.version} onClick={() => void open(version.version)}>
                        {t(`${prefix}versionValue`, { version: version.version })}
                    </Button>}
                {version.version === current && <Badge variant="outline">{t(`${prefix}currentVersion`)}</Badge>}
                <Badge variant="secondary">{t(`${prefix}origin.${version.origin}`)}</Badge>
                {version.scan && <Badge variant={scanNeedsConfirmation(version.scan) ? 'destructive' : 'outline'}>
                    {t(`${prefix}scanBadge.${version.scan.verdict}`)}
                </Badge>}
                <span className="flex-1 text-xs text-muted-foreground">{new Date(version.createdAtMs).toLocaleString(i18n.language)}</span>
                <span className="text-xs tabular-nums text-muted-foreground">{t(`${prefix}bytes`, { value: number(version.sizeBytes) })}</span>
            </li>)}
        </ul>
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
        {selected && <div aria-label={t(`${prefix}versionValue`, { version: selected.info.version })} role="region" className="space-y-3 rounded-md border p-3">
            {selected.info.scan && <ScanReport scan={selected.info.scan} recorded />}
            <p className="text-sm font-medium">{t(`${prefix}diffToCurrent`)}</p>
            <SkillDiffList diff={selected.diffToCurrent} />
            <Disclosure title={<span className="text-sm font-medium">SKILL.md</span>}>
                <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/30 p-3 text-xs">{selected.skillMd}</pre>
            </Disclosure>
            <SkillFileList files={selected.files} />
        </div>}
    </section>;
}

function SkillDetailDialog({ api, skillId, onClose, onEdit, onUpload, onChanged, onConflict }: {
    api: SkillApi; skillId: string;
    onClose(): void; onEdit(detail: SkillDetail): void; onUpload(skill: SkillSummary): void;
    onChanged(): void; onConflict(): void;
}) {
    const { t } = useTranslation();
    const [detail, setDetail] = useState<SkillDetail | null>(null);
    const [error, setError] = useState<Failure | null>(null);
    const [confirmDelete, setConfirmDelete] = useState(false);
    const [busy, setBusy] = useState(false);
    useEffect(() => {
        let live = true;
        api.detail(skillId).then(value => { if (live) setDetail(value); })
            .catch(cause => { if (live) setError({ text: skillErrorText(t, cause), conflict: false }); });
        return () => { live = false; };
    }, [api, skillId, t]);
    const remove = async () => {
        if (!detail || busy) return;
        setBusy(true);
        try {
            await api.remove(skillId, detail.summary.revision);
            onChanged();
        } catch (cause) {
            setConfirmDelete(false); setBusy(false);
            setError({ text: skillErrorText(t, cause), conflict: isSkillConflict(cause) });
        }
    };
    const skill = detail?.summary;
    const facts: [string, string | null | undefined][] = detail ? [
        [t(`${prefix}version`), t(`${prefix}versionValue`, { version: detail.summary.currentVersion })],
        [t(`${prefix}compatibility`), detail.compatibility],
        [t(`${prefix}license`), detail.license],
        [t(`${prefix}allowedTools`), detail.allowedTools],
        ...Object.entries(detail.metadata ?? {}),
    ] : [];
    return <Dialog open onOpenChange={open => { if (!open && !busy) onClose(); }}>
        <DialogContent aria-describedby={undefined} className="max-h-[90vh] max-w-3xl overflow-y-auto">
            <DialogHeader>
                <DialogTitle className="font-mono">{skill?.name ?? t(`${prefix}loading`)}</DialogTitle>
            </DialogHeader>
            {skill && detail && <div className="space-y-4">
                <div className="flex flex-wrap gap-2"><SkillBadges skill={skill} /></div>
                <p className="whitespace-pre-wrap text-sm">{skill.description}</p>
                <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1 text-sm">
                    {facts.filter(([, value]) => value).map(([label, value]) => <div key={label} className="contents">
                        <dt className="text-muted-foreground">{label}</dt><dd className="whitespace-pre-wrap break-words">{value}</dd>
                    </div>)}
                </dl>
                <section className="space-y-2">
                    <p className="text-sm font-medium">{t(`${prefix}files`)}</p>
                    <SkillFileList files={detail.files} />
                </section>
                <Disclosure title={<span className="text-sm font-medium">SKILL.md</span>}>
                    <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/30 p-3 text-xs">{detail.skillMd}</pre>
                </Disclosure>
                <SkillVersionHistory api={api} skillId={skillId} current={detail.summary.currentVersion} versions={detail.versions} />
            </div>}
            {error && <div role="alert" className="space-y-2 text-sm text-destructive">
                <p>{error.text}</p>
                {error.conflict && <Button variant="outline" size="sm" onClick={onConflict}>{t(`${prefix}reload`)}</Button>}
            </div>}
            {skill && detail && <DialogFooter className="gap-2 sm:justify-between">
                <Button variant="ghost" className="text-destructive" disabled={busy} onClick={() => setConfirmDelete(true)}>{t(`${prefix}delete`)}</Button>
                <div className="flex flex-wrap gap-2">
                    <Button variant="outline" disabled={busy} onClick={() => onUpload(skill)}>{t(`${prefix}uploadVersion`)}</Button>
                    <Button disabled={busy} onClick={() => onEdit(detail)}>{t(`${prefix}edit`)}</Button>
                </div>
            </DialogFooter>}
            <AlertDialog open={confirmDelete} onOpenChange={open => { if (!open && !busy) setConfirmDelete(false); }}>
                <AlertDialogContent>
                    <AlertDialogHeader>
                        <AlertDialogTitle>{t(`${prefix}deleteTitle`, { name: skill?.name ?? '' })}</AlertDialogTitle>
                        <AlertDialogDescription>{t(`${prefix}deleteDescription`)}</AlertDialogDescription>
                    </AlertDialogHeader>
                    <AlertDialogFooter>
                        <AlertDialogCancel disabled={busy}>{t('common.cancel')}</AlertDialogCancel>
                        <Button variant="destructive" disabled={busy} onClick={() => void remove()}>{t(`${prefix}delete`)}</Button>
                    </AlertDialogFooter>
                </AlertDialogContent>
            </AlertDialog>
        </DialogContent>
    </Dialog>;
}

function BundledSkillDialog({ skill, onClose }: { skill: BundledSkillView; onClose(): void }) {
    const { t } = useTranslation();
    return <Dialog open onOpenChange={open => { if (!open) onClose(); }}>
        <DialogContent aria-describedby={undefined} className="max-h-[90vh] max-w-3xl overflow-y-auto">
            <DialogHeader><DialogTitle className="font-mono">{skill.name}</DialogTitle></DialogHeader>
            <Badge variant="secondary" className="w-fit">{t(`${prefix}bundled`)}</Badge>
            <p className="whitespace-pre-wrap text-sm">{skill.description}</p>
            <SkillFileList files={skill.files} />
            <pre className="max-h-96 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/30 p-3 text-xs">{skill.skillMd}</pre>
        </DialogContent>
    </Dialog>;
}

/**
 * Platform or personal skill management. The platform page also lists the
 * bundled skills, which can only be viewed and switched on or off.
 */
export function AssistantSkillsSettings({ scope }: { scope: SkillScopeKind }) {
    const { t } = useTranslation();
    const api = skillApi(scope);
    const [skills, setSkills] = useState<SkillSummary[] | null>(null);
    const [bundled, setBundled] = useState<BundledSkillList | null>(null);
    const [proposals, setProposals] = useState<SkillProposalView[]>([]);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<Failure | null>(null);
    const [overlay, setOverlay] = useState<Overlay | null>(null);
    const epoch = useRef(0);

    const load = useCallback(async () => {
        const own = ++epoch.current;
        setBusy(true); setError(null); setOverlay(null);
        try {
            const [list, bundledList, pending] = await Promise.all([
                api.list(),
                scope === 'platform' ? loadBundled() : Promise.resolve(null),
                // Proposals are secondary; their failure must not hide the skill list.
                scope === 'personal' ? loadPendingProposals().catch(() => []) : Promise.resolve([]),
            ]);
            if (own !== epoch.current) return;
            setSkills(list); setBundled(bundledList); setProposals(pending);
        } catch (cause) {
            if (own === epoch.current) setError({ text: skillErrorText(t, cause), conflict: false });
        } finally {
            if (own === epoch.current) setBusy(false);
        }
    }, [api, scope, t]);
    useEffect(() => { void load(); return () => { epoch.current += 1; }; }, [load]);

    const mutate = async (action: () => Promise<void>) => {
        if (busy) return;
        const own = ++epoch.current;
        setBusy(true); setError(null);
        try { await action(); }
        catch (cause) { if (own === epoch.current) setError({ text: skillErrorText(t, cause), conflict: isSkillConflict(cause) }); }
        finally { if (own === epoch.current) setBusy(false); }
    };
    const replace = (skill: SkillSummary) => setSkills(previous => {
        const list = previous ?? [];
        return list.some(item => item.skillId === skill.skillId)
            ? list.map(item => item.skillId === skill.skillId ? skill : item) : [...list, skill];
    });
    const toggle = (skill: SkillSummary, enabled: boolean) => mutate(async () => {
        replace((await api.update(skill.skillId, { expectedRevision: skill.revision, enabled })).skill);
    });
    const toggleBundled = (name: string, enabled: boolean) => mutate(async () => {
        if (!bundled) return;
        const body = await setBundledSkill(name, { expectedRevision: bundled.revision, enabled });
        if (!body.success || !body.data) throw new Error('Bundled skill update failed');
        setBundled(body.data);
    });
    const saved = (skill: SkillSummary) => { replace(skill); setOverlay(null); };
    const reload = () => { void load(); };
    const bundledView = overlay?.kind === 'bundled' ? bundled?.skills.find(skill => skill.name === overlay.name) : undefined;
    const empty = skills != null && !skills.length && !bundled?.skills.length;

    return <div className="mx-auto max-w-3xl space-y-4 p-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-xl font-semibold">{t(`${prefix}${scope}.title`)}</h1>
            <div className="flex gap-2">
                <Button variant="outline" disabled={busy} onClick={reload}>{t(`${prefix}reload`)}</Button>
                <Button disabled={busy || skills == null} onClick={() => setOverlay({ kind: 'import' })}>{t(`${prefix}import`)}</Button>
            </div>
        </div>
        <p className="text-sm text-muted-foreground">{t(`${prefix}${scope}.scope`)}</p>
        {error && <div role="alert" className="flex flex-wrap items-center gap-2 text-sm text-destructive">
            <span>{error.text}</span>
            {error.conflict && <Button variant="outline" size="sm" onClick={reload}>{t(`${prefix}reload`)}</Button>}
        </div>}
        {!!proposals.length && <section aria-label={t(`${prefix}proposal.pendingTitle`)} className="space-y-2">
            <h2 className="text-sm font-medium">{t(`${prefix}proposal.pendingTitle`)}</h2>
            <SkillProposalCards proposals={proposals} awaitingOwner onRefresh={reload} onDecided={reload} />
        </section>}
        {(skills?.length || bundled?.skills.length) ? <Card><CardContent className="py-0">
            <ul className="divide-y">
                {skills?.map(skill => <SkillRow key={skill.skillId} name={skill.name} description={skill.description}
                    badges={<SkillBadges skill={skill} />} enabled={skill.enabled} busy={busy}
                    onToggle={enabled => void toggle(skill, enabled)} onOpen={() => setOverlay({ kind: 'detail', skillId: skill.skillId })} />)}
                {bundled?.skills.map(skill => <SkillRow key={`bundled:${skill.name}`} name={skill.name} description={skill.description}
                    badges={<Badge variant="outline">{t(`${prefix}bundled`)}</Badge>} enabled={skill.enabled} busy={busy}
                    onToggle={enabled => void toggleBundled(skill.name, enabled)} onOpen={() => setOverlay({ kind: 'bundled', name: skill.name })} />)}
            </ul>
        </CardContent></Card> : empty && <p className="text-sm text-muted-foreground">{t(`${prefix}empty`)}</p>}
        {overlay?.kind === 'detail' && <SkillDetailDialog api={api} skillId={overlay.skillId} onClose={() => setOverlay(null)}
            onEdit={detail => setOverlay({ kind: 'edit', detail })} onUpload={base => setOverlay({ kind: 'import', base })}
            onChanged={reload} onConflict={reload} />}
        {bundledView && <BundledSkillDialog skill={bundledView} onClose={() => setOverlay(null)} />}
        {overlay?.kind === 'import' && <AssistantSkillImportDialog api={api} base={overlay.base}
            onClose={() => setOverlay(null)} onSaved={saved} onConflict={reload} />}
        {overlay?.kind === 'edit' && <AssistantSkillEditor api={api} detail={overlay.detail}
            onClose={() => setOverlay(null)} onSaved={saved} onConflict={reload} />}
    </div>;
}

export function PlatformAssistantSkills() {
    return <AssistantSkillsSettings scope="platform" />;
}
