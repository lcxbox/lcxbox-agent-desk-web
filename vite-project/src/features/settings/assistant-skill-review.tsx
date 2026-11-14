import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { DiffView } from '@/components/ui/diff-view';
import type { SkillFileDiff, SkillFileMeta, SkillPreview, SkillScanResult } from '@/services/types';

const prefix = 'pages.assistantSkills.';

/** A requested scan that found something or could not finish needs an explicit "save anyway". */
export function scanNeedsConfirmation(scan: SkillScanResult | null | undefined): boolean {
    return scan?.verdict === 'suspicious' || scan?.verdict === 'failed';
}

export function SkillFileList({ files }: { files: SkillFileMeta[] }) {
    const { t, i18n } = useTranslation();
    if (!files.length) return null;
    return <ul className="divide-y rounded-md border text-sm">
        {files.map(file => <li key={file.path} className="flex flex-wrap items-center gap-2 px-3 py-1.5">
            <span className="min-w-0 flex-1 break-all font-mono text-xs">{file.path}</span>
            <Badge variant="secondary">{t(`${prefix}kind.${file.kind}`)}</Badge>
            {file.language && <Badge variant="outline">{file.language}</Badge>}
            <span className="text-xs tabular-nums text-muted-foreground">
                {t(`${prefix}bytes`, { value: new Intl.NumberFormat(i18n.language).format(file.sizeBytes) })}
            </span>
        </li>)}
    </ul>;
}

/** `recorded` shows the scan saved with a version rather than one deciding a save. */
export function ScanReport({ scan, recorded = false }: { scan: SkillScanResult | null | undefined; recorded?: boolean }) {
    const { t } = useTranslation();
    const verdict = scan?.verdict ?? 'unavailable';
    const serious = scanNeedsConfirmation(scan);
    return <section aria-label={t(`${prefix}scan.title`)} className="space-y-2">
        <p role={serious ? 'alert' : 'status'} className={serious ? 'text-sm font-medium text-destructive' : 'text-sm text-muted-foreground'}>
            {t(recorded ? `${prefix}scanBadge.${verdict}` : `${prefix}scan.${verdict}`)}
        </p>
        {!!scan?.findings.length && <ul className="space-y-1 text-sm">
            {scan.findings.map((finding, index) => <li key={index} className="rounded-md border border-destructive/40 px-3 py-2">
                <p className="font-mono text-xs">{finding.line ? `${finding.path}:${finding.line}` : finding.path}</p>
                <p><span className="font-medium">{finding.category}</span> · {finding.reason}</p>
            </li>)}
        </ul>}
        {!!scan?.unscanned?.length && <p className="text-sm text-muted-foreground">
            {t(`${prefix}scan.unscanned`, { files: scan.unscanned.join(', ') })}
        </p>}
    </section>;
}

/** Validation, AI scan and changes of a package that has not been saved yet. */
export function SkillPreviewReport({ preview, scanRequested, showChanges }: { preview: SkillPreview; scanRequested: boolean; showChanges: boolean }) {
    const { t } = useTranslation();
    return <div className="space-y-4">
        {scanRequested && <ScanReport scan={preview.scan} />}
        <section className="space-y-2">
            <p className="text-sm font-medium">{t(`${prefix}files`)}</p>
            <SkillFileList files={preview.files} />
            {!!preview.skipped.length && <ul aria-label={t(`${prefix}skipped`)} className="space-y-1 text-sm text-amber-700 dark:text-amber-400">
                {preview.skipped.map(file => <li key={file.path}>
                    {t(`${prefix}skippedFile`, { path: file.path, reason: t(`${prefix}skip.${file.reason}`) })}
                </li>)}
            </ul>}
            {preview.warnings.map(warning => <p key={warning} className="text-sm text-amber-700 dark:text-amber-400">{t(`${prefix}warning.${warning}`)}</p>)}
        </section>
        {showChanges && <section className="space-y-3" aria-label={t(`${prefix}changes`)}>
            <p className="text-sm font-medium">{t(`${prefix}changes`)}</p>
            <SkillDiffList diff={preview.diff} />
        </section>}
    </div>;
}

/** Server-computed per-file changes. */
export function SkillDiffList({ diff }: { diff: SkillFileDiff[] }) {
    const { t } = useTranslation();
    if (!diff.length) return <p className="text-sm text-muted-foreground">{t(`${prefix}noChanges`)}</p>;
    return <div className="space-y-3">
        {diff.map(file => <div key={file.path} className="space-y-1">
            <p className="flex items-center gap-2 text-sm">
                <span className="break-all font-mono text-xs">{file.path}</span>
                <Badge variant="outline">{t(`${prefix}change.${file.change}`)}</Badge>
            </p>
            <DiffView hunks={file.hunks} aria-label={file.path} />
            {file.truncated && <p className="text-xs text-muted-foreground">{t(`${prefix}diffTruncated`)}</p>}
        </div>)}
    </div>;
}
