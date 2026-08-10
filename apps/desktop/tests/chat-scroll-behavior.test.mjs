import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  createChatScrollSnapshot,
  observeAtBottomSignal,
  observeChatContent,
  observeChatScroll,
  requestLatest,
  shouldFollowChatOutput,
} from "../src/components/chat/scrollState.ts";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("streaming height growth keeps an already-pinned transcript pinned", () => {
  const initial = createChatScrollSnapshot(500);
  const resized = observeChatScroll(initial, {
    scrollTop: 500,
    scrollHeight: 1200,
    clientHeight: 600,
  });

  assert.equal(resized.atBottom, false, "the physical bottom may move after item resize");
  assert.equal(resized.pinnedToBottom, true, "resize drift is not user intent");
  assert.equal(shouldFollowChatOutput(observeChatContent(resized)), true);
});

test("an intentional upward scroll disables following and marks later output unseen", () => {
  const initial = createChatScrollSnapshot(600);
  const scrolledUp = observeChatScroll(initial, {
    scrollTop: 420,
    scrollHeight: 1200,
    clientHeight: 600,
  });
  const withOutput = observeChatContent(scrolledUp);

  assert.equal(scrolledUp.pinnedToBottom, false);
  assert.equal(shouldFollowChatOutput(withOutput), false);
  assert.equal(withOutput.hasUnseenContent, true);
});

test("a small upward scroll inside the bottom threshold preserves reader intent", () => {
  const initial = createChatScrollSnapshot(600);
  const scrolledUp = observeChatScroll(initial, {
    scrollTop: 580,
    scrollHeight: 1200,
    clientHeight: 600,
  });

  assert.equal(scrolledUp.atBottom, true, "the viewport remains physically near the bottom");
  assert.equal(scrolledUp.pinnedToBottom, false, "upward intent must win over the threshold");

  const afterPositiveSignal = observeAtBottomSignal(scrolledUp, true);
  assert.equal(
    afterPositiveSignal.pinnedToBottom,
    false,
    "a stale positive Virtuoso signal must not re-enable following"
  );

  const withOutput = observeChatContent(afterPositiveSignal);
  assert.equal(shouldFollowChatOutput(withOutput), false);
  assert.equal(withOutput.hasUnseenContent, true);
});

test("scrolling back near the bottom resumes following and clears unseen output", () => {
  const away = {
    ...createChatScrollSnapshot(420),
    atBottom: false,
    pinnedToBottom: false,
    hasUnseenContent: true,
  };
  const returned = observeChatScroll(away, {
    scrollTop: 570,
    scrollHeight: 1200,
    clientHeight: 600,
  });

  assert.equal(returned.atBottom, true);
  assert.equal(returned.pinnedToBottom, true);
  assert.equal(returned.hasUnseenContent, false);
});

test("a negative Virtuoso bottom signal cannot override user intent by itself", () => {
  const pinned = createChatScrollSnapshot(600);
  const drifted = observeAtBottomSignal(pinned, false);

  assert.equal(drifted.atBottom, false);
  assert.equal(drifted.pinnedToBottom, true);
});

test("the new-messages action explicitly restores follow mode", () => {
  const away = {
    ...createChatScrollSnapshot(250),
    atBottom: false,
    pinnedToBottom: false,
    hasUnseenContent: true,
  };
  const latest = requestLatest(away);

  assert.deepEqual(
    { atBottom: latest.atBottom, pinned: latest.pinnedToBottom, unseen: latest.hasUnseenContent },
    { atBottom: true, pinned: true, unseen: false }
  );
});

test("MessageList wires intent state to Virtuoso without forced smooth streaming scrolls", async () => {
  const source = await read("src/components/chat/MessageList.tsx");

  assert.match(source, /scrollerRef=\{setScrollerRef\}/);
  assert.match(source, /followOutput=\{false\}/);
  assert.match(source, /observeChatContent\(scrollRef\.current\)/);
  assert.match(source, /scrollToLatest\("auto"\)/);
  assert.match(source, /New messages/);
  assert.doesNotMatch(source, /initialTopMostItemIndex/);
});
