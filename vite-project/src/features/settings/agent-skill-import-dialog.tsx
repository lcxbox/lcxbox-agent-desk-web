import { useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Textarea } from '@/components/ui/textarea';
import type { SkillPackageInput, SkillPreview, SkillSummary } from '@/services/types';
import { fileBase64, isSkillConflict, skillErrorText, useSkillScanAvailable, type SkillApi } from './agent-skills-api';
import { scanNeedsConfirmation, SkillPreviewReport } from './agent-skill-review';

const prefix = 'pages.agentSkills.';

/**
 * Imports a new skill, or uploads a new version of `base`. The package is
 * always validated (and optionally AI-scanned) by the preview endpoint first,
 * and only the previewed package is saved.
 */
export function AgentSkillImportDialog({ api, base, onClose, onSaved, onConflict }: {
    api: SkillApi;
    base?: SkillSummary;
    onClose(): void;
    onSaved(skill: SkillSummary): void;
    onConflict(): void;
}) {
    const { t } = useTranslation();
    const [mode, setMode] = useState<'zip' | 'text'>('zip');
    const [file, setFile] = useState<File | null>(null);
    const [skillMd, setSkillMd] = useState('');
    const [scanChoice, setScan] = useState(true);
    const scanAvailable = useSkillScanAvailable(api);
    const scan = scanChoice && scanAvailable;
    const [checked, setChecked] = useState<{ preview: SkillPreview; pkg: SkillPackageInput; scan: boolean } | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<{ text: string; conflict: boolean } | null>(null);
    const fileInput = useRef<HTMLInputElement>(null);
    const ready = mode === 'zip' ? !!file : !!skillMd.trim();
    const edit = () => { setChecked(null); setError(null); };
    const fail = (cause: unknown) => setError({ text: skillErrorText(t, cause), conflict: isSkillConflict(cause) });

    const check = async () => {
        if (busy || !ready) return;
        setBusy(true); setError(null);
        try {
            const pkg: SkillPackageInput = mode === 'zip' && file ? { zipBase64: await fileBase64(file) } : { skillMd };
            const preview = await api.preview({ package: pkg, aiScan: scan }, base?.skillId);
            setChecked({ preview, pkg, scan });
        } catch (cause) { fail(cause); }
        finally { setBusy(false); }
    };
    const save = async () => {
        if (busy || !checked) return;
        setBusy(true); setError(null);
        try {
            const scanId = checked.preview.scanId ?? undefined;
            const saved = base
                ? await api.update(base.skillId, { expectedRevision: base.revision, package: checked.pkg, origin: 'imported', scanId })
                : await api.create({ package: checked.pkg, scanId });
            onSaved(saved.skill);
        } catch (cause) { fail(cause); setBusy(false); }
    };
    const confirm = checked?.scan && scanNeedsConfirmation(checked.preview.scan);

    return <Dialog open onOpenChange={open => { if (!open && !busy) onClose(); }}>
        <DialogContent aria-describedby={undefined} className="max-h-[90vh] max-w-2xl overflow-y-auto">
            <DialogHeader>
                <DialogTitle>{base ? t(`${prefix}uploadVersionTitle`, { name: base.name }) : t(`${prefix}importTitle`)}</DialogTitle>
            </DialogHeader>
            <Tabs value={mode} onValueChange={value => { if (value === 'zip' || value === 'text') { setMode(value); edit(); } }}>
                <TabsList>
                    <TabsTrigger value="zip" disabled={busy}>{t(`${prefix}zipTab`)}</TabsTrigger>
                    <TabsTrigger value="text" disabled={busy}>{t(`${prefix}textTab`)}</TabsTrigger>
                </TabsList>
                <TabsContent value="zip" className="flex items-center gap-3">
                    <Button type="button" variant="outline" disabled={busy} onClick={() => fileInput.current?.click()}>{t(`${prefix}chooseZip`)}</Button>
                    <span className="min-w-0 truncate text-sm text-muted-foreground">{file?.name}</span>
                    <input ref={fileInput} type="file" accept=".zip,application/zip" className="hidden" data-testid="skill-zip-input"
                        onChange={event => { setFile(event.target.files?.[0] ?? null); edit(); event.target.value = ''; }} />
                </TabsContent>
                <TabsContent value="text">
                    <Textarea aria-label="SKILL.md" rows={12} className="font-mono text-xs" disabled={busy} value={skillMd}
                        placeholder={t(`${prefix}skillMdPlaceholder`)} onChange={event => { setSkillMd(event.target.value); edit(); }} />
                </TabsContent>
            </Tabs>
            <div className="flex items-center gap-3">
                <Switch id="skill-import-scan" checked={scan} disabled={busy || !scanAvailable} aria-describedby={scanAvailable ? undefined : 'skill-import-scan-unavailable'} onCheckedChange={value => { setScan(value); edit(); }} />
                <Label htmlFor="skill-import-scan">{t(`${prefix}aiScan`)}</Label>
                {!scanAvailable && <span id="skill-import-scan-unavailable" className="text-xs text-muted-foreground">{t(`${prefix}scanUnavailableReason`)}</span>}
            </div>
            {checked && <SkillPreviewReport preview={checked.preview} scanRequested={checked.scan} showChanges={!!base} />}
            {error && <div role="alert" className="space-y-2 text-sm text-destructive">
                <p>{error.text}</p>
                {error.conflict && <Button variant="outline" size="sm" onClick={onConflict}>{t(`${prefix}reload`)}</Button>}
            </div>}
            <DialogFooter>
                <Button variant="outline" disabled={busy} onClick={onClose}>{t('common.cancel')}</Button>
                {!checked
                    ? <Button disabled={busy || !ready} onClick={() => void check()}>{t(`${prefix}check`)}</Button>
                    : <Button variant={confirm ? 'destructive' : 'default'} disabled={busy} onClick={() => void save()}>
                        {t(confirm ? `${prefix}saveAnyway` : 'common.save')}
                    </Button>}
            </DialogFooter>
        </DialogContent>
    </Dialog>;
}
