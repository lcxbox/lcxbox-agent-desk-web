import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
    Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog';
import { startLocalRecovery, finishLocalRecovery, cancelLocalRecovery } from '@/services/clients';
import { errorCodeOf } from '@/lib/desk-error-i18n';
import { deskErrorCodeEnum } from '@/services/types';

export function LocalPasswordRecovery({ onSuccess }: { onSuccess: () => void }) {
    const { t } = useTranslation();
    const [location, setLocation] = useState<{ config_file?: string | null } | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [code, setCode] = useState('');
    const [password, setPassword] = useState('');
    const [confirm, setConfirm] = useState('');
    const host = window.location.hostname;
    if (!['localhost', '[::1]'].includes(host) && !/^127(?:\.\d{1,3}){3}$/.test(host)) return null;

    function clear() {
        setLocation(null);
        setPassword('');
        setCode('');
        setConfirm('');
        setError('');
    }
    function failure(reason: unknown) {
        switch (errorCodeOf(reason)) {
            case deskErrorCodeEnum.TOO_MANY_ATTEMPTS: return t('pages.recovery.wait');
            case deskErrorCodeEnum.INVALID_OR_EXPIRED_TOKEN: return t('pages.recovery.invalid');
            case deskErrorCodeEnum.PERMISSION_ERROR: return t('pages.recovery.permission');
            default: return t('pages.recovery.failure');
        }
    }
    async function start() {
        setBusy(true);
        setError('');
        try {
            const result = await startLocalRecovery();
            if (!result.data) throw new Error();
            setLocation(result.data);
        } catch (reason) {
            setError(failure(reason));
        } finally {
            setBusy(false);
        }
    }
    async function close() {
        if (busy) return;
        setBusy(true);
        try {
            await cancelLocalRecovery();
            clear();
        } catch (reason) {
            setError(failure(reason));
        } finally {
            setBusy(false);
        }
    }
    async function submit(event: FormEvent) {
        event.preventDefault();
        if (password !== confirm) {
            setError(t('pages.recovery.mismatch'));
            return;
        }
        setBusy(true);
        try {
            await finishLocalRecovery({ code, new_password: password });
            clear();
            onSuccess();
        } catch (reason) {
            setError(failure(reason));
        } finally {
            setBusy(false);
        }
    }
    return <>
        <Button type="button" variant="link" disabled={busy} onClick={start}>
            {t('pages.login.forgotPassword')}
        </Button>
        {!location && error && <p role="alert">{error}</p>}
        <Dialog open={location !== null} onOpenChange={(open) => { if (!open) void close(); }}>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>{t('pages.recovery.title')}</DialogTitle>
                    <DialogDescription className="break-words">
                        {location?.config_file ? t('pages.recovery.file', { path: location.config_file }) : t('pages.recovery.window')}
                    </DialogDescription>
                </DialogHeader>
                <form className="grid gap-3" onSubmit={submit}>
                    <Label htmlFor="recovery-code">{t('pages.recovery.code')}</Label>
                    <Input id="recovery-code" autoComplete="one-time-code" inputMode="numeric" pattern="[0-9]{8}" maxLength={8} value={code} onChange={e => setCode(e.target.value)} required />
                    <Label htmlFor="recovery-password">{t('pages.recovery.password')}</Label>
                    <Input id="recovery-password" type="password" autoComplete="new-password" value={password} onChange={e => setPassword(e.target.value)} required />
                    <Label htmlFor="recovery-confirm">{t('pages.recovery.confirm')}</Label>
                    <Input id="recovery-confirm" type="password" autoComplete="new-password" value={confirm} onChange={e => setConfirm(e.target.value)} required />
                    {error && <p role="alert">{error}</p>}
                    <DialogFooter>
                        <Button type="button" variant="outline" disabled={busy} onClick={close}>{t('common.cancel')}</Button>
                        <Button type="submit" disabled={busy}>{t('pages.recovery.title')}</Button>
                    </DialogFooter>
                </form>
            </DialogContent>
        </Dialog>
    </>;
}
