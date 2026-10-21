import {
  sessionNeedsInput,
  type Block,
  type Session,
} from "../../sessions/model/session";
import { operatorUserPrompt } from "../../sessions/model/operatorCommand";

type Exchange = { user: Block; assistants: Block[] };

export type SessionReadOptions = {
  before?: string;
  limit?: number;
  maxChars?: number;
};

function capped(text: string, maxChars: number) {
  const trimmed = text.trim();
  return {
    text: trimmed.slice(0, maxChars),
    truncated: trimmed.length > maxChars,
  };
}

/** Show conversation prose only, with a stable cursor for older exchanges. */
export function sessionConversationPage(
  session: Session,
  options: SessionReadOptions = {},
) {
  const limit = options.limit ?? 3;
  if (!Number.isInteger(limit) || limit < 1 || limit > 3)
    throw new Error("limit must be an integer from 1 to 3");
  const maxChars = options.maxChars ?? 1200;
  if (!Number.isInteger(maxChars) || maxChars < 200 || maxChars > 6000)
    throw new Error("maxChars must be an integer from 200 to 6000");

  const exchanges: Exchange[] = [];
  for (const block of session.blocks) {
    if (
      block.role === "user" &&
      (!block.internal || block.monoSessionCompletion) &&
      !block.draft
    ) {
      exchanges.push({ user: block, assistants: [] });
    } else if (
      block.role === "assistant" &&
      !block.internal &&
      block.text.trim() &&
      exchanges.length > 0
    ) {
      exchanges[exchanges.length - 1].assistants.push(block);
    }
  }

  const end = options.before
    ? exchanges.findIndex((exchange) => exchange.user.id === options.before)
    : exchanges.length;
  if (end < 0) throw new Error("before is not a turn ID in this session");
  const start = Math.max(0, end - limit);
  const selected = exchanges.slice(start, end);
  return {
    sessionId: session.id,
    title: session.title,
    busy: !!session.busy,
    hasDraft: session.blocks.some(
      (block) => block.role === "user" && block.draft,
    ),
    turns: selected.map(({ user, assistants }) => ({
      turnId: user.id,
      user: capped(
        user.monoSessionCompletion
          ? user.monoSessionCompletion.sessionCount
            ? `MonoCode: results from ${user.monoSessionCompletion.sessionCount} sessions`
            : `MonoCode: session ${user.monoSessionCompletion.status}: ${user.monoSessionCompletion.title}`
          : operatorUserPrompt(user),
        maxChars,
      ),
      assistant: assistants.length
        ? capped(assistants[assistants.length - 1].text, maxChars)
        : null,
      earlierAssistantMessages: Math.max(0, assistants.length - 1),
    })),
    nextBefore: start > 0 ? selected[0]?.user.id : null,
  };
}

/** Where the turn submitted under `appRequestId` stands, with its final reply. */
export function askedTurn(
  session: Session,
  appRequestId: string,
  maxChars: number,
) {
  const start = session.blocks.findIndex(
    (block) => block.role === "user" && block.appRequestId === appRequestId,
  );
  if (start < 0) throw new Error("The asked turn is no longer in this session");
  const user = session.blocks[start];
  const rest = session.blocks.slice(start + 1);
  const next = rest.findIndex(
    (block) =>
      block.role === "user" &&
      !block.draft &&
      (!block.internal || block.monoSessionCompletion),
  );
  const own = next < 0 ? rest : rest.slice(0, next);
  const latest = next < 0;
  if (latest && (session.busy || sessionNeedsInput(session)))
    return {
      turnId: user.id,
      status: sessionNeedsInput(session) ? "needsInput" : "running",
      reply: null,
    };
  const reply = [...own]
    .reverse()
    .find(
      (block) =>
        block.role === "assistant" && !block.internal && block.text.trim(),
    );
  const failure = own.find((block) => block.notice === "error");
  return {
    turnId: user.id,
    status: failure
      ? "failed"
      : own.some((block) => block.notice === "interrupt")
        ? "stopped"
        : "done",
    ...(failure ? { error: capped(failure.text, maxChars).text } : {}),
    reply: reply ? capped(reply.text, maxChars) : null,
  };
}
