import { useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { X } from 'lucide-react';
import { useToast } from '@/hooks/use-toast';
import { readAgentDraft, writeAgentDraft } from './agent-draft';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { respondToOwnerQuestion } from '@/services/clients';
import type { OwnerAnswer, OwnerQuestionAction, OwnerQuestionView } from '@/services/types';

export function AgentOwnerQuestion({ session, wait, refresh }: {
    session: string;
    wait: OwnerQuestionView;
    refresh(): void;
}) {
    const { t } = useTranslation();
    const { toast } = useToast();
    const draftKey = JSON.stringify([session, wait.waitId]);
    const questions = wait.prompt.kind === 'questions' ? wait.prompt.questions : [];
    const [answers, setAnswers] = useState<(OwnerAnswer | null)[]>(() => {
        try {
            const saved = JSON.parse(readAgentDraft(draftKey));
            if (Array.isArray(saved) && saved.length === questions.length) return saved;
        } catch { /* A fresh question has no saved answers. */ }
        return questions.map(() => null);
    });
    const [reply, setReply] = useState(() => wait.prompt.kind === 'follow_up' ? readAgentDraft(draftKey) : '');
    const [sending, setSending] = useState(false);
    const [closed, setClosed] = useState(false);
    const [failed, setFailed] = useState(false);
    const attempt = useRef<{ signature: string; id: string } | null>(null);
    const pending = useRef(false);
    const setAnswer = (index: number, answer: OwnerAnswer) => {
        const next = answers.map((item, i) => i === index ? answer : item);
        setAnswers(next);
        writeAgentDraft(draftKey, JSON.stringify(next));
    };
    const send = async (action: OwnerQuestionAction) => {
        if (pending.current) return;
        pending.current = true;
        setSending(true);
        setFailed(false);
        const signature = JSON.stringify(action);
        const attemptKey = draftKey + '/submission';
        if (!attempt.current) {
            try { attempt.current = JSON.parse(readAgentDraft(attemptKey)); } catch { /* First submission. */ }
        }
        if (attempt.current?.signature !== signature) attempt.current = { signature, id: v4() };
        writeAgentDraft(attemptKey, JSON.stringify(attempt.current));
        try {
            const result = await respondToOwnerQuestion({ session, waitId: wait.waitId,
                clientRequestId: attempt.current.id, action });
            if (!result.success) throw new Error();
            if (result.data === 'timed_out') toast({ title: t('pages.aiAgent.ownerQuestionTimedOut') });
            writeAgentDraft(draftKey, '');
            writeAgentDraft(attemptKey, '');
            setClosed(true);
        } catch {
            setFailed(true);
        } finally {
            pending.current = false;
            setSending(false);
            refresh();
        }
    };
    if (closed) return null;
    return <section className="space-y-3 rounded-md border bg-card p-3" aria-label={t('pages.aiAgent.ownerQuestions')}>
        {wait.prompt.kind === 'questions' ? <>
            <div className="flex items-center justify-between gap-2">
                <strong>{t('pages.aiAgent.ownerQuestions')}</strong>
                <Button type="button" size="icon" variant="ghost" disabled={sending} aria-label={t('pages.aiAgent.memory.close')}
                    onClick={() => void send({ kind: 'decline' })}><X /></Button>
            </div>
            {questions.map((q, i) => {
                const answer = answers[i];
                return <fieldset key={i} disabled={sending} className="space-y-2">
                    <legend className="whitespace-pre-wrap text-sm font-medium">{q.question}</legend>
                    <div className="flex flex-wrap gap-2">
                        {q.options?.map(option => <Button type="button" size="sm" key={option}
                            aria-pressed={answer?.kind === 'choice' && answer.value === option}
                            variant={answer?.kind === 'choice' && answer.value === option ? 'default' : 'outline'}
                            onClick={() => setAnswer(i, { kind: 'choice', value: option })}>{option}</Button>)}
                        <Button type="button" size="sm" variant={answer?.kind === 'skipped' ? 'secondary' : 'ghost'}
                            aria-pressed={answer?.kind === 'skipped'} onClick={() => setAnswer(i, { kind: 'skipped' })}>
                            {t('pages.aiAgent.skipQuestion')}</Button>
                    </div>
                    <Textarea aria-label={q.question} rows={1} value={answer?.kind === 'text' ? answer.value : ''}
                        onChange={e => setAnswer(i, { kind: 'text', value: e.target.value })} />
                </fieldset>;
            })}
            <Button type="button" size="sm" disabled={sending || answers.some(a => !a || (a.kind === 'text' && (!a.value.trim() || Array.from(a.value).length > 1024)))}
                onClick={() => void send({ kind: 'answer', answers: answers.filter((a): a is OwnerAnswer => !!a) })}>{t('pages.aiAgent.send')}</Button>
        </> : <>
            <label className="block space-y-2"><span>{wait.prompt.text}</span>
                <Textarea value={reply} disabled={sending} onChange={e => { setReply(e.target.value); writeAgentDraft(draftKey, e.target.value); }} /></label>
            <Button type="button" size="sm" disabled={sending || !reply.trim() || Array.from(reply).length > 1024}
                onClick={() => void send({ kind: 'reply', text: reply })}>{t('pages.aiAgent.subagents.sendReply')}</Button>
        </>}
        {wait.deadlineUnixMs != null && <p className="text-xs text-muted-foreground">{t('pages.aiAgent.questionDeadline', { time: new Date(wait.deadlineUnixMs).toLocaleTimeString() })}</p>}
        {failed && <p role="alert" className="text-sm text-destructive">{t('pages.aiAgent.directories.submitUnavailable')}</p>}
    </section>;
}
