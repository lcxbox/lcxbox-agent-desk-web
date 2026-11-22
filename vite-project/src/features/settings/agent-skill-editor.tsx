import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import type { SkillDetail, SkillFileInput, SkillPackageInput, SkillPreview, SkillSummary } from '@/services/types';
import { isSkillConflict, skillErrorText, useSkillScanAvailable, type SkillApi } from './agent-skills-api';
import { scanNeedsConfirmation, SkillPreviewReport } from './agent-skill-review';

const prefix = 'pages.agentSkills.';

/**
 * Online editor for the canonical `SKILL.md` and the text files. It always
 * sends the full package; the name inside `SKILL.md` stays fixed and the server
 * rejects a renamed package.
 */
export function AgentSkillEditor({ api, detail, onClose, onSaved, onConflict }: {
    api: SkillApi;
    detail: SkillDetail;
    onClose(): void;
    onSaved(skill: SkillSummary): void;
    onConflict(): void;
}) {
    const { t } = useTranslation();
    const skill = detail.summary;
    const [skillMd, setSkillMd] = useState(detail.skillMd);
    const [files, setFiles] = useState<SkillFileInput[]>(() => detail.files.map(file => ({ path: file.path, text: file.text })));
    const [newPath, setNewPath] = useState('');
    const [scanChoice, setScan] = useState(false);
    const scanAvailable = useSkillScanAvailable(api);
    const scan = scanChoice && scanAvailable;
    const [checked, setChecked] = useState<{ preview: SkillPreview; pkg: SkillPackageInput; scan: boolean } | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<{ text: string; conflict: boolean } | null>(null);
    const changed = () => { setChecked(null); setError(null); };
    const fail = (cause: unknown) => setError({ text: skillErrorText(t, cause), conflict: isSkillConflict(cause) });
    const setFile = (index: number, text: string) => {
        setFiles(previous => previous.map((file, position) => position === index ? { ...file, text } : file));
        changed();
    };
    const path = newPath.trim();
    const addable = !!path && path !== 'SKILL.md' && !files.some(file => file.path === path);

    const preview = async () => {
        if (busy) return;
        setBusy(true); setError(null);
        try {
            const pkg: SkillPackageInput = { skillMd, files };
            setChecked({ preview: await api.preview({ package: pkg, aiScan: scan }, skill.skillId), pkg, scan });
        } catch (cause) { fail(cause); }
        finally { setBusy(false); }
    };
    const save = async () => {
        if (busy || !checked) return;
        setBusy(true); setError(null);
        try {
            const saved = await api.update(skill.skillId, {
                expectedRevision: skill.revision, package: checked.pkg, origin: 'edited',
                scanId: checked.preview.scanId ?? undefined,
            });
            onSaved(saved.skill);
        } catch (cause) { fail(cause); setBusy(false); }
    };
    const confirm = checked?.scan && scanNeedsConfirmation(checked.preview.scan);

    return <Dialog open onOpenChange={open => { if (!open && !busy) onClose(); }}>
        <DialogContent aria-describedby={undefined} className="max-h-[90vh] max-w-3xl overflow-y-auto">
            <DialogHeader><DialogTitle>{t(`${prefix}editTitle`, { name: skill.name })}</DialogTitle></DialogHeader>
            {checked ? <SkillPreviewReport preview={checked.preview} scanRequested={checked.scan} showChanges /> : <div className="space-y-4">
                <div className="space-y-1">
                    <Label htmlFor="skill-edit-skill-md">SKILL.md</Label>
                    <Textarea id="skill-edit-skill-md" rows={14} className="font-mono text-xs" disabled={busy} value={skillMd}
                        onChange={event => { setSkillMd(event.target.value); changed(); }} />
                </div>
                {files.map((file, index) => <div key={file.path} className="space-y-1">
                    <div className="flex items-center justify-between gap-2">
                        <Label htmlFor={`skill-edit-file-${index}`} className="break-all font-mono text-xs">{file.path}</Label>
                        <Button variant="ghost" size="sm" disabled={busy} aria-label={t(`${prefix}removeFile`, { path: file.path })}
                            onClick={() => { setFiles(previous => previous.filter((_, position) => position !== index)); changed(); }}>
                            {t(`${prefix}remove`)}
                        </Button>
                    </div>
                    <Textarea id={`skill-edit-file-${index}`} rows={6} className="font-mono text-xs" disabled={busy} value={file.text}
                        onChange={event => setFile(index, event.target.value)} />
                </div>)}
                <div className="flex gap-2">
                    <Input aria-label={t(`${prefix}newFilePath`)} placeholder="references/notes.md" value={newPath} disabled={busy}
                        onChange={event => setNewPath(event.target.value)} />
                    <Button variant="outline" disabled={busy || !addable}
                        onClick={() => { setFiles(previous => [...previous, { path, text: '' }]); setNewPath(''); changed(); }}>
                        {t(`${prefix}addFile`)}
                    </Button>
                </div>
                <div className="flex items-center gap-3">
                    <Switch id="skill-edit-scan" checked={scan} disabled={busy || !scanAvailable} aria-describedby={scanAvailable ? undefined : 'skill-edit-scan-unavailable'} onCheckedChange={value => { setScan(value); changed(); }} />
                    <Label htmlFor="skill-edit-scan">{t(`${prefix}aiScan`)}</Label>
                    {!scanAvailable && <span id="skill-edit-scan-unavailable" className="text-xs text-muted-foreground">{t(`${prefix}scanUnavailableReason`)}</span>}
                </div>
            </div>}
            {error && <div role="alert" className="space-y-2 text-sm text-destructive">
                <p>{error.text}</p>
                {error.conflict && <Button variant="outline" size="sm" onClick={onConflict}>{t(`${prefix}reload`)}</Button>}
            </div>}
            <DialogFooter>
                {checked
                    ? <>
                        <Button variant="outline" disabled={busy} onClick={changed}>{t(`${prefix}backToEdit`)}</Button>
                        <Button variant={confirm ? 'destructive' : 'default'} disabled={busy || !checked.preview.diff.length} onClick={() => void save()}>
                            {t(confirm ? `${prefix}saveAnyway` : 'common.save')}
                        </Button>
                    </>
                    : <>
                        <Button variant="outline" disabled={busy} onClick={onClose}>{t('common.cancel')}</Button>
                        <Button disabled={busy} onClick={() => void preview()}>{t(`${prefix}reviewChanges`)}</Button>
                    </>}
            </DialogFooter>
        </DialogContent>
    </Dialog>;
}
