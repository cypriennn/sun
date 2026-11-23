(() => {
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  };
  const app = $('app');
  if (!app) return;

  let chatId = app.dataset.chatId || '';
  let controller = null;
  let staged = [];
  let temporaryHistory = [];
  let pendingReplyChat = '';
  let temporaryMode = false;

  const mainPane = $('main-pane'), sidebar = $('sidebar'), backdrop = $('backdrop'), menuBtn = $('menu-btn');
  const runtimePanel = $('runtime-panel'), runtimeBtn = $('runtime-btn'), runtimeError = $('runtime-error');
  const messagesEl = $('messages'), errorEl = $('error-banner');
  const form = $('composer'), input = $('prompt-input');
  const sendBtn = $('send-btn'), stopBtn = $('stop-btn');
  const attachBtn = $('attach-btn'), fileInput = $('file-input'), attSlot = $('attachment-slot');
  const sharedList = $('shared-list'), sharedCount = $('shared-count'), sharedBar = $('shared-bar');
  const sharedUploadBtn = $('shared-upload-btn'), sharedFileInput = $('shared-file-input');
  const mcpActiveCount = $('mcp-active-count');
  const modelSelect = $('model-select'), modelTagsEl = $('model-tags');
  const reasoningGroup = $('reasoning-group'), reasoningSelect = $('reasoning-select');
  const compactBtn = $('compact-btn');
  const tempToggle = $('temporary-chat');
  const schedules = $('schedules'), scheduleForm = $('schedule-form'), scheduleList = $('schedule-list');
  const titleInput = $('chat-title'), folderSelect = $('chat-folder');
  const sysInput = $('system-prompt'), saveSysBtn = $('save-sys');
  const newFolderBtn = $('new-folder-btn'), folderForm = $('folder-form'), folderNameInput = $('folder-name-input');

  const showError = (msg) => {
    const inRuntime = runtimePanel.classList.contains('open');
    errorEl.textContent = inRuntime ? '' : msg;
    runtimeError.textContent = inRuntime ? msg : '';
    errorEl.hidden = inRuntime || !msg;
    runtimeError.hidden = !inRuntime || !msg;
  };
  const scrollBottom = () => { messagesEl.scrollTop = messagesEl.scrollHeight; };
  const flashBtn = (btn, label) => {
    const orig = btn.textContent;
    btn.textContent = label;
    setTimeout(() => { btn.textContent = orig; }, 900);
  };
  const fmtBytes = (n) => n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : n >= 1024 ? `${(n / 1024).toFixed(1)} KB` : `${n} B`;
  const fileHref = (cid, name) => `/api/chats/${encodeURIComponent(cid)}/files/${encodeURIComponent(name)}`;

  const apiJson = async (url, method, body, keepalive = false) => {
    const res = await fetch(url, {
      method,
      headers: { 'content-type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined,
      keepalive,
    });
    if (!res.ok) {
      const data = await res.json().catch(() => ({}));
      throw new Error(data.error || `HTTP ${res.status}`);
    }
    return res.status === 204 ? null : res.json();
  };

  const syncTemporary = () => {
    temporaryMode = !chatId && tempToggle.checked;
    attachBtn.disabled = temporaryMode;
    sharedUploadBtn.disabled = temporaryMode;
    titleInput.readOnly = temporaryMode;
    titleInput.value = temporaryMode ? 'Temporary chat' : 'New chat';
  };
  tempToggle.checked = false;
  tempToggle.addEventListener('change', syncTemporary);
  window.addEventListener('pagehide', () => {
    if (!temporaryMode) return;
    controller?.abort();
    temporaryHistory = [];
    messagesEl.replaceChildren();
    input.value = '';
    sysInput.value = '';
  });
  window.addEventListener('pageshow', (event) => {
    if (event.persisted && temporaryMode) {
      tempToggle.checked = false;
      tempToggle.disabled = false;
      syncTemporary();
    }
  });

  const refreshSchedules = async () => {
    const res = await fetch('/api/schedules');
    if (!res.ok) throw new Error(`Could not load schedules (HTTP ${res.status}).`);
    scheduleList.innerHTML = await res.text();
    for (const time of scheduleList.querySelectorAll('time')) time.textContent = new Date(time.dateTime).toLocaleString();
  };
  schedules.addEventListener('toggle', () => {
    if (schedules.open) refreshSchedules().catch((err) => showError(err.message));
  });
  scheduleForm?.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (temporaryMode) { showError('Leave temporary chat before scheduling a prompt.'); return; }
    const data = new FormData(scheduleForm);
    const submit = scheduleForm.querySelector('button');
    submit.disabled = true;
    try {
      await apiJson('/api/schedules', 'POST', {
        prompt: data.get('prompt'), run_at: new Date(data.get('run_at')).toISOString(),
        every_minutes: data.get('every_minutes') ? Number(data.get('every_minutes')) : null,
        model: modelSelect?.value || null, reasoning: reasoningSelect?.value ?? null,
      });
      scheduleForm.reset();
      await refreshSchedules();
    } catch (err) { showError(err.message); }
    finally { submit.disabled = false; }
  });

  const updateMcpCount = () => {
    if (!mcpActiveCount) return;
    mcpActiveCount.textContent = String(document.querySelectorAll('.mcp-toggle:checked').length);
  };

  const saveMcpKeyInput = async (inputEl, saveBtn) => {
    const val = inputEl.value.trim();
    if (!val) return;
    await apiJson('/api/mcp', 'POST', { name: inputEl.dataset.mcp, auth_key: val });
    inputEl.value = '';
    inputEl.placeholder = 'Key set (type to update)';
    const row = inputEl.closest('.mcp-row');
    const clearB = row?.querySelector('.mcp-clear-key');
    if (clearB) clearB.hidden = false;
    if (saveBtn) flashBtn(saveBtn, 'Saved');
  };

  const flushPendingMcpKeys = async () => {
    for (const inputEl of document.querySelectorAll('.mcp-key')) {
      if (inputEl.value.trim()) await saveMcpKeyInput(inputEl, inputEl.closest('.mcp-row')?.querySelector('.mcp-save-btn'));
    }
  };

  const pendingToolSaves = new Set();
  runtimePanel.addEventListener('change', async ({ target }) => {
    if (!target.matches('.tool-toggle, .mcp-toggle')) return;
    const { tool, mcp } = target.dataset;
    const enabled = target.checked;
    showError('');
    target.disabled = true;
    const save = apiJson(tool ? '/api/tools' : '/api/mcp', 'POST', { name: tool || mcp, enabled }, true);
    pendingToolSaves.add(save);
    try { await save; }
    catch (err) { target.checked = !enabled; showError(err.message); }
    finally {
      pendingToolSaves.delete(save);
      target.disabled = false;
      if (mcp) updateMcpCount();
    }
  });

  document.addEventListener('keydown', async (e) => {
    if (e.key === 'Enter' && e.target.classList.contains('mcp-key')) {
      e.preventDefault();
      try { await saveMcpKeyInput(e.target, e.target.closest('.mcp-row')?.querySelector('.mcp-save-btn')); }
      catch (err) { showError(err.message); }
    }
  });

  const renderStaged = () => {
    attSlot.replaceChildren();
    attSlot.hidden = !staged.length;
    for (const name of staged) {
      const chip = el('span', 'shared-item');
      const rm = el('button', 'icon-btn danger', '×');
      rm.type = 'button';
      rm.addEventListener('click', () => {
        staged = staged.filter((s) => s !== name);
        renderStaged();
      });
      chip.append(el('span', 'shared-link', name), rm);
      attSlot.append(chip);
    }
  };

  const renderSharedItems = (items) => {
    sharedList.replaceChildren();
    sharedCount.textContent = String(items.length);
    for (const sf of items) {
      const row = el('div', 'shared-item');
      row.dataset.name = sf.name;
      const link = el('a', 'shared-link', sf.name);
      link.href = fileHref(chatId, sf.name);
      link.download = sf.name;
      const sizePrefix = sf.kind === 'image' ? 'vision · ' : sf.kind === 'file' ? 'file · ' : '';
      const addB = el('button', 'icon-btn', '+');
      addB.type = 'button';
      addB.dataset.action = 'attach-shared';
      addB.dataset.name = sf.name;
      const delB = el('button', 'icon-btn danger', '×');
      delB.type = 'button';
      delB.dataset.action = 'delete-shared';
      delB.dataset.name = sf.name;
      row.append(link, el('span', 'tag', sizePrefix + fmtBytes(sf.size)), addB, delB);
      sharedList.append(row);
    }
  };

  const refreshShared = async () => {
    if (!chatId) return;
    const items = await apiJson(`/api/chats/${encodeURIComponent(chatId)}/files`, 'GET');
    renderSharedItems(items);
  };

  const activateChat = (id, title) => {
    if (!chatId && id) {
      chatId = id;
      app.dataset.chatId = id;
      if (saveSysBtn) saveSysBtn.hidden = false;
      compactBtn.disabled = false;
      $('temporary-label').hidden = true;
      history.replaceState(null, '', `/c/${encodeURIComponent(id)}`);
    }
    if (title && titleInput) titleInput.value = title;
    ensureSidebarEntry(id, title || 'New chat');
  };

  const ensureChat = async () => {
    if (chatId) return chatId;
    const folder = folderSelect ? (folderSelect.value || null) : null;
    const created = await apiJson('/api/chats', 'POST', { folder });
    activateChat(created.id, created.title);
    return created.id;
  };

  const uploadFiles = async (files, stageForMsg) => {
    if (!files || !files.length) return;
    if (temporaryMode) { showError('Temporary chats do not support attachments.'); return; }
    tempToggle.disabled = true;
    showError('');
    const prevTxt = attachBtn.textContent;
    attachBtn.textContent = '…';
    try {
      const fd = new FormData();
      for (const f of files) fd.append('file', f);
      const id = await ensureChat();
      const res = await fetch(`/api/chats/${encodeURIComponent(id)}/files`, { method: 'POST', body: fd });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
      if (stageForMsg) {
        for (const sf of data) if (!staged.includes(sf.name)) staged.push(sf.name);
        renderStaged();
      } else if (sharedBar) {
        sharedBar.open = true;
      }
      await refreshShared();
    } catch (err) {
      showError(err.message);
    } finally {
      attachBtn.textContent = prevTxt;
      if (!controller && !chatId) tempToggle.disabled = false;
    }
  };

  if (attachBtn && fileInput) {
    attachBtn.addEventListener('click', () => fileInput.click());
    fileInput.addEventListener('change', () => { uploadFiles(fileInput.files, true); fileInput.value = ''; });
  }
  if (sharedUploadBtn && sharedFileInput) {
    sharedUploadBtn.addEventListener('click', () => sharedFileInput.click());
    sharedFileInput.addEventListener('change', () => { uploadFiles(sharedFileInput.files, false); sharedFileInput.value = ''; });
  }
  if (mainPane) {
    mainPane.addEventListener('dragover', (e) => { if (e.dataTransfer?.types?.includes('Files')) { e.preventDefault(); mainPane.classList.add('drag-over'); } });
    mainPane.addEventListener('dragleave', (e) => { if (!mainPane.contains(e.relatedTarget)) mainPane.classList.remove('drag-over'); });
    mainPane.addEventListener('drop', (e) => {
      mainPane.classList.remove('drag-over');
      if (e.dataTransfer?.files?.length) { e.preventDefault(); uploadFiles(e.dataTransfer.files, true); }
    });
  }
  if (input) {
    input.addEventListener('paste', (e) => {
      const files = [...(e.clipboardData?.items || [])].filter((i) => i.kind === 'file').map((i) => i.getAsFile()).filter(Boolean);
      if (files.length) { e.preventDefault(); uploadFiles(files, true); }
    });
  }

  const syncModelMeta = (useInitial) => {
    const opt = modelSelect && modelSelect.selectedOptions[0];
    if (!opt) return;
    modelTagsEl.replaceChildren(
      ...JSON.parse(opt.dataset.tags || '[]').map(([k, v]) => el('span', 'tag', k === 'price' ? v : `${k}: ${v}`))
    );
    const rTags = JSON.parse(opt.dataset.reasoning || '[]');
    const desired = useInitial ? (reasoningSelect.dataset.initial || '') : (opt.dataset.defaultReasoning || '');
    reasoningSelect.replaceChildren();
    reasoningGroup.hidden = !rTags.length;
    if (!rTags.length) return;
    reasoningSelect.append(new Option('auto', ''), ...rTags.map((r) => new Option(r, r)));
    reasoningSelect.value = rTags.includes(desired) ? desired : '';
  };

  const persistSelection = async () => {
    if (temporaryMode) return;
    const model = modelSelect ? modelSelect.value : '';
    const reasoning = reasoningSelect?.value || '';
    try {
      if (chatId) await apiJson(`/api/chats/${encodeURIComponent(chatId)}`, 'PATCH', { model, reasoning });
      else await apiJson('/api/preferences', 'POST', { model, reasoning });
    } catch (e) { showError(e.message); }
  };

  if (modelSelect) {
    syncModelMeta(true);
    modelSelect.addEventListener('change', () => { syncModelMeta(false); persistSelection(); });
  }
  if (reasoningSelect) reasoningSelect.addEventListener('change', persistSelection);

  const narrowLayout = matchMedia('(max-width: 1024px)');
  let openDrawer = null;
  const setDrawer = (next, restoreFocus = true) => {
    const previous = openDrawer;
    openDrawer = next;
    sidebar.classList.toggle('open', next === sidebar);
    runtimePanel.classList.toggle('open', next === runtimePanel);
    menuBtn.setAttribute('aria-expanded', next === sidebar);
    runtimeBtn.setAttribute('aria-expanded', next === runtimePanel);
    backdrop.classList.toggle('open', !!next);
    mainPane.inert = !!next;
    sidebar.inert = next === runtimePanel;
    runtimePanel.inert = next === sidebar;
    if (next) next.focus({ preventScroll: true });
    else if (previous && restoreFocus) (previous === sidebar ? menuBtn : runtimeBtn).focus();
  };
  menuBtn.addEventListener('click', () => setDrawer(sidebar));
  runtimeBtn.addEventListener('click', () => setDrawer(runtimePanel));
  $('runtime-close').addEventListener('click', () => setDrawer(null));
  backdrop.addEventListener('click', () => setDrawer(null));
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && openDrawer) setDrawer(null);
  });
  narrowLayout.addEventListener('change', () => { if (openDrawer) setDrawer(null, false); });

  if (newFolderBtn && folderForm) {
    newFolderBtn.addEventListener('click', () => {
      folderForm.hidden = !folderForm.hidden;
      if (!folderForm.hidden) folderNameInput.focus();
    });
    folderForm.addEventListener('submit', async (e) => {
      e.preventDefault();
      const name = folderNameInput.value.trim();
      if (!name) return;
      try { await apiJson('/api/folders', 'POST', { name }); location.reload(); }
      catch (err) { showError(err.message); }
    });
  }

  if (titleInput) {
    titleInput.addEventListener('change', async () => {
      const title = titleInput.value.trim();
      if (!chatId || !title) return;
      try {
        await apiJson(`/api/chats/${encodeURIComponent(chatId)}`, 'PATCH', { title });
        const link = document.querySelector(`.chat-item[data-chat-id="${CSS.escape(chatId)}"] .chat-link`);
        if (link) link.textContent = title;
      } catch (err) { showError(err.message); }
    });
  }
  if (folderSelect) {
    folderSelect.addEventListener('change', async () => {
      if (!chatId) return;
      try { await apiJson(`/api/chats/${encodeURIComponent(chatId)}`, 'PATCH', { folder: folderSelect.value }); location.reload(); }
      catch (err) { showError(err.message); }
    });
  }
  if (saveSysBtn && sysInput) {
    saveSysBtn.addEventListener('click', async () => {
      if (!chatId) return;
      try {
        await apiJson(`/api/chats/${encodeURIComponent(chatId)}`, 'PATCH', { system_prompt: sysInput.value });
        flashBtn(saveSysBtn, 'Saved');
      } catch (err) { showError(err.message); }
    });
  }
  if (stopBtn) stopBtn.addEventListener('click', () => controller && controller.abort());
  if (compactBtn) {
    compactBtn.addEventListener('click', async () => {
      if (!chatId || controller) return;
      showError('');
      const prev = compactBtn.textContent;
      compactBtn.textContent = 'Compacting…';
      compactBtn.disabled = true;
      try {
        const res = await apiJson(`/api/chats/${encodeURIComponent(chatId)}/compact`, 'POST');
        messagesEl.replaceChildren(
          createMsgNode('user', res.user_msg),
          createMsgNode('assistant', res.assistant_msg),
        );
        scrollBottom();
      } catch (err) {
        showError(err.message);
      } finally {
        compactBtn.textContent = prev;
        compactBtn.disabled = false;
      }
    });
  }

  const replyToTool = (ev, answer) => apiJson(`/api/chats/${encodeURIComponent(pendingReplyChat)}/answer`, 'POST', {
    question_id: ev.question_id, answer,
  });

  const renderLocationRequest = (assistantEl, ev) => {
    const card = el('div', 'ask-card');
    card.append(el('div', 'ask-q', 'Share approximate location with the assistant?'));
    const options = el('div', 'ask-opts');
    const add = (label, getAnswer) => {
      const button = el('button', '', label);
      button.type = 'button';
      button.addEventListener('click', async () => {
        for (const b of options.children) b.disabled = true;
        try {
          await replyToTool(ev, await getAnswer());
          card.remove();
        } catch (err) {
          showError(err.message);
          for (const b of options.children) b.disabled = false;
        }
      });
      options.append(button);
    };
    if (navigator.geolocation && window.isSecureContext) {
      add('Share approximate coordinates', async () => {
        return JSON.stringify(await new Promise((resolve, reject) => navigator.geolocation.getCurrentPosition(
          ({ coords }) => resolve({ latitude: +coords.latitude.toFixed(2), longitude: +coords.longitude.toFixed(2) }),
          () => reject(new Error('Location unavailable.')), { timeout: 10000, enableHighAccuracy: false },
        )));
      });
    } else card.append(el('p', '', 'Browser location is unavailable on this connection.'));
    add('Decline', () => 'decline');
    card.append(options);
    assistantEl.querySelector('.msg-body').before(card);
  };

  const renderAskCard = (assistantEl, ev) => {
    const card = el('div', 'ask-card');
    card.append(el('div', 'ask-q', ev.question));
    let sending = false;
    const sendAnswer = async (ans, display) => {
      const text = (ans || '').trim();
      if (!text || !pendingReplyChat || sending) return;
      sending = true;
      try {
        await replyToTool(ev, text);
        card.replaceChildren(el('span', 'tag', `Answered: ${display ?? text}`));
      } catch (err) { showError(err.message); sending = false; }
    };
    if (ev.mode === 'multiple') {
      const form = el('form');
      const submit = el('button', '', 'Reply');
      submit.type = 'submit';
      submit.disabled = true;
      for (const opt of ev.options) {
        const label = el('label');
        const checkbox = el('input');
        checkbox.type = 'checkbox';
        checkbox.value = opt;
        label.append(checkbox, ` ${opt}`);
        const row = el('div');
        row.append(label);
        form.append(row);
      }
      form.append(submit);
      form.addEventListener('change', () => {
        submit.disabled = !form.querySelector('input:checked');
      });
      form.addEventListener('submit', (e) => {
        e.preventDefault();
        const selected = Array.from(form.querySelectorAll('input:checked'), (input) => input.value);
        if (selected.length) sendAnswer(JSON.stringify(selected), selected.join(', '));
      });
      card.append(form);
    } else if (ev.mode !== 'text' && ev.options?.length) {
      const opts = el('div', 'ask-opts');
      for (const opt of ev.options) {
        const b = el('button', '', opt);
        b.type = 'button';
        b.addEventListener('click', () => sendAnswer(opt));
        opts.append(b);
      }
      card.append(opts);
    }
    assistantEl.querySelector('.msg-body').before(card);
    const f = el('div', 'ask-form');
    const inp = el('input');
    inp.type = 'text';
    inp.placeholder = 'Type your answer…';
    const sub = el('button', '', 'Reply');
    sub.type = 'button';
    sub.addEventListener('click', () => sendAnswer(inp.value));
    inp.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); sendAnswer(inp.value); }
    });
    f.append(inp, sub);
    card.append(f);
    if (ev.mode !== 'single' && ev.mode !== 'multiple') inp.focus();
  };

  const renderMsgAttachments = (cid, atts) => {
    if (!atts || !atts.length) return null;
    const wrap = el('div', 'msg-atts');
    for (const att of atts) {
      const href = fileHref(cid, att.name);
      if (att.kind === 'image') {
        const a = el('a', 'att-img-link');
        a.href = href; a.target = '_blank'; a.rel = 'noopener';
        const img = el('img', 'att-img');
        img.src = href; img.alt = att.name;
        a.append(img);
        wrap.append(a);
      } else {
        const a = el('a', 'att-chip', att.name);
        a.href = href; a.download = att.name;
        wrap.append(a);
      }
    }
    return wrap;
  };

  const createMsgNode = (role, msg, pending = false) => {
    const art = el('article', `msg msg-${role}`);
    art.dataset.msgId = msg.id;
    const card = el('div', 'msg-card');
    const label = el('div', 'msg-role', role === 'user' ? 'You' : 'Assistant');
    if (role === 'assistant') {
      if (msg.model_name) label.append(el('span', 'msg-model', ` (${msg.model_name})`));
      const activity = el('div', 'msg-activity'), reasoning = el('details', 'activity reasoning');
      reasoning.append(el('summary'), el('div', 'activity-text'));
      activity.append(reasoning);
      card.append(activity);
      updateReasoning(card, pending, msg.reasoning || '');
    }
    const attNode = renderMsgAttachments(chatId, msg.attachments);
    if (attNode) card.append(attNode);
    const body = el('div', 'msg-body');
    body.innerHTML = msg.html || '';
    const rawTa = el('textarea', 'msg-raw');
    rawTa.hidden = true;
    rawTa.value = msg.raw || '';
    card.append(body, rawTa);

    const actions = el('div', 'msg-actions');
    actions.hidden = pending;
    const mkAct = (act, txt) => { const b = el('button', '', txt); b.type = 'button'; b.dataset.action = act; return b; };
    actions.append(mkAct('copy-msg', 'Copy'));
    if (role === 'user') actions.append(mkAct('edit-msg', 'Edit'));
    actions.append(mkAct('retry-msg', 'Retry'));
    if (!temporaryMode && app.dataset.canFork === 'true') actions.append(mkAct('fork-chat', 'Fork here'));
    art.append(label, card, actions);
    return art;
  };

  const updateReasoning = (node, thinking, text) => {
    const details = node.querySelector('.reasoning'), body = details.querySelector('.activity-text');
    if (text !== undefined) body.textContent = text;
    details.hidden = !thinking && !body.textContent;
    details.querySelector('summary').textContent = thinking ? 'Thinking…' : 'Reasoning';
  };

  const removeFromNode = (startNode, inclusive) => {
    let cur = inclusive ? startNode : startNode.nextElementSibling;
    while (cur) {
      const next = cur.nextElementSibling;
      if (cur.classList.contains('msg')) cur.remove();
      cur = next;
    }
  };

  const ensureSidebarEntry = (id, title) => {
    const existing = document.querySelector(`.chat-item[data-chat-id="${CSS.escape(id)}"]`);
    if (existing) { existing.querySelector('.chat-link').textContent = title; return; }
    const rootList = $('root-chats');
    if (!rootList) return;
    document.querySelectorAll('.chat-item.active').forEach((n) => n.classList.remove('active'));
    const row = el('div', 'chat-item active');
    row.dataset.chatId = id;
    const a = el('a', 'chat-link', title);
    a.href = `/c/${encodeURIComponent(id)}`;
    const btns = el('span', 'chat-btns');
    const del = el('button', 'icon-btn danger', '×');
    del.type = 'button';
    del.dataset.action = 'delete-chat';
    del.dataset.id = id;
    btns.append(del);
    row.append(a, btns);
    rootList.prepend(row);
  };

  const runStream = async (action, targetMsgEl) => {
    showError('');
    const emptyEl = $('empty-state');
    if (emptyEl) emptyEl.remove();

    controller = new AbortController();
    tempToggle.disabled = true;
    sendBtn.hidden = true;
    stopBtn.hidden = false;
    let assistantEl = null, toolEl = null, modelName = '';
    const appendAssistant = (id) => {
      assistantEl = createMsgNode('assistant', { id, model_name: modelName }, true);
      messagesEl.append(assistantEl);
    };
    const finishTool = (output) => {
      toolEl.querySelector('summary').textContent = `Tool: ${toolEl.dataset.toolName}`;
      toolEl.querySelector('.tool-output').textContent = output;
      toolEl = null;
    };

    try {
      await Promise.all(pendingToolSaves);
      if (!temporaryMode) await flushPendingMcpKeys();
      const res = await fetch('/api/stream', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          chat_id: chatId || null,
          folder: folderSelect ? (folderSelect.value || null) : null,
          system_prompt: sysInput ? sysInput.value : null,
          model: modelSelect ? modelSelect.value : null,
          reasoning: reasoningSelect?.value ?? null,
          temporary: temporaryMode ? temporaryHistory : null,
          browser_time_zone: document.querySelector('[data-tool="current_timestamp"]:checked') ? {
            timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
            utc_offset_minutes: -new Date().getTimezoneOffset(),
          } : null,
          action,
        }),
        signal: controller.signal,
      });
      if (!res.ok) {
        const errData = await res.json().catch(() => ({}));
        throw new Error(errData.error || `HTTP ${res.status}`);
      }

      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf('\n')) !== -1) {
          const line = buf.slice(0, idx).trim();
          buf = buf.slice(idx + 1);
          if (!line) continue;
          const ev = JSON.parse(line);
          const follow = messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 40;
          if (ev.type === 'init') {
            pendingReplyChat = ev.chat_id;
            if (temporaryMode) {
              const at = temporaryHistory.findIndex(({ id }) => id === action.message_id);
              if (at >= 0) temporaryHistory.length = at + (action.type === 'retry' && temporaryHistory[at].role === 'user' ? 1 : 0);
              if (ev.user_msg) temporaryHistory.push({ id: ev.user_msg.id, role: 'user', content: ev.user_msg.raw });
            } else activateChat(ev.chat_id, ev.title);
            if (action.type === 'send' && ev.user_msg) {
              messagesEl.append(createMsgNode('user', ev.user_msg));
            } else if (action.type === 'edit' && targetMsgEl && ev.user_msg) {
              removeFromNode(targetMsgEl, false);
              targetMsgEl.querySelector('.msg-body').innerHTML = ev.user_msg.html;
              targetMsgEl.querySelector('.msg-raw').value = ev.user_msg.raw;
            } else if (action.type === 'retry' && targetMsgEl) {
              removeFromNode(targetMsgEl, !targetMsgEl.classList.contains('msg-user'));
            }
            modelName = ev.model_name;
            appendAssistant(ev.assistant_id);
            scrollBottom();
          } else if (ev.type === 'continue') {
            appendAssistant(ev.assistant_id);
          } else if (ev.type === 'tool_start' && assistantEl) {
            updateReasoning(assistantEl, false);
            const activity = assistantEl.querySelector('.msg-activity');
            activity.insertAdjacentHTML('beforeend', ev.html);
            toolEl = activity.lastElementChild;
          } else if (ev.type === 'tool_end' && toolEl) {
            finishTool(ev.output);
          } else if (ev.type === 'ask_user' && assistantEl) {
            renderAskCard(assistantEl, ev);
            scrollBottom();
          } else if (ev.type === 'current_location' && assistantEl) {
            renderLocationRequest(assistantEl, ev);
            scrollBottom();
          } else if (ev.type === 'shared_updated') {
            refreshShared().catch(() => {});
          } else if ((ev.type === 'render' || ev.type === 'done') && assistantEl) {
            assistantEl.querySelector('.msg-body').innerHTML = ev.html;
            updateReasoning(assistantEl, ev.type === 'render' && !ev.html, ev.reasoning);
            if (ev.type === 'done' && (ev.raw || ev.reasoning)) {
              if (temporaryMode) temporaryHistory.push({ id: ev.id, role: 'assistant', content: ev.raw });
              assistantEl.querySelector('.msg-raw').value = ev.raw;
              assistantEl.querySelector('.msg-actions').hidden = false;
              assistantEl = null;
            }
          } else if (ev.type === 'error') {
            showError(ev.message);
          }
          if (follow) scrollBottom();
        }
      }
    } catch (err) {
      if (err.name !== 'AbortError') showError(err.message);
    } finally {
      if (toolEl) finishTool('No result received.');
      if (assistantEl) {
        updateReasoning(assistantEl, false);
        if (!assistantEl.querySelector('.msg-body').innerHTML && !assistantEl.querySelector('.activity:not([hidden])')) {
          assistantEl.remove();
        }
      }
      controller = null;
      pendingReplyChat = '';
      if (!chatId && !temporaryHistory.length) tempToggle.disabled = false;
      sendBtn.hidden = false;
      stopBtn.hidden = true;
    }
  };

  document.addEventListener('click', async (e) => {
    const copyCodeBtn = e.target.closest('.copy-code');
    if (copyCodeBtn) {
      const codeEl = copyCodeBtn.closest('.code-block')?.querySelector('pre code');
      if (codeEl) { await navigator.clipboard.writeText(codeEl.textContent || ''); flashBtn(copyCodeBtn, 'Copied'); }
      return;
    }
    const btn = e.target.closest('[data-action]');
    if (!btn) return;
    const act = btn.dataset.action, msgEl = btn.closest('.msg');
    if (act === 'copy-msg' && msgEl) {
      await navigator.clipboard.writeText(msgEl.querySelector('.msg-raw')?.value || '');
      flashBtn(btn, 'Copied');
    } else if (act === 'fork-chat' && msgEl && chatId && !controller) {
      btn.disabled = true;
      try {
        const fork = await apiJson(`/api/chats/${encodeURIComponent(chatId)}/fork`, 'POST', { through: msgEl.dataset.msgId });
        location.href = `/c/${encodeURIComponent(fork.id)}`;
      } catch (err) { showError(err.message); btn.disabled = false; }
    } else if (act === 'delete-schedule') {
      try { await apiJson(`/api/schedules/${encodeURIComponent(btn.dataset.id)}`, 'DELETE'); await refreshSchedules(); }
      catch (err) { showError(err.message); }
    } else if (act === 'retry-msg' && msgEl && !controller) {
      await runStream({ type: 'retry', message_id: msgEl.dataset.msgId }, msgEl);
    } else if (act === 'edit-msg' && msgEl && !controller && !msgEl.querySelector('.edit-box')) {
      const bodyEl = msgEl.querySelector('.msg-body');
      const box = el('div', 'edit-box'), ta = el('textarea'), btns = el('div', 'edit-btns');
      const saveB = el('button', '', 'Save & Send'), cancelB = el('button', '', 'Cancel');
      saveB.type = cancelB.type = 'button';
      ta.value = msgEl.querySelector('.msg-raw')?.value || '';
      btns.append(saveB, cancelB);
      box.append(ta, btns);
      bodyEl.hidden = true;
      bodyEl.after(box);
      ta.focus();
      cancelB.addEventListener('click', () => { box.remove(); bodyEl.hidden = false; });
      saveB.addEventListener('click', async () => {
        const nextText = ta.value.trim();
        if (!nextText) return;
        box.remove();
        bodyEl.hidden = false;
        await runStream({ type: 'edit', message_id: msgEl.dataset.msgId, content: nextText }, msgEl);
      });
    } else if (act === 'save-mcp-key') {
      const inputEl = btn.closest('.mcp-row')?.querySelector('.mcp-key');
      if (inputEl) {
        try { await saveMcpKeyInput(inputEl, btn); }
        catch (err) { showError(err.message); }
      }
    } else if (act === 'clear-mcp-key') {
      const name = btn.dataset.mcp;
      try {
        await apiJson('/api/mcp', 'POST', { name, auth_key: '' });
        btn.hidden = true;
        const inputEl = btn.closest('.mcp-row')?.querySelector('.mcp-key');
        if (inputEl) { inputEl.value = ''; inputEl.placeholder = 'Bearer auth key'; }
      } catch (err) { showError(err.message); }
    } else if (act === 'attach-shared') {
      const name = btn.dataset.name;
      if (name && !staged.includes(name)) {
        staged.push(name);
        renderStaged();
        if (openDrawer === runtimePanel) setDrawer(null, false);
        input.focus();
      }
    } else if (act === 'delete-shared') {
      const name = btn.dataset.name;
      if (!chatId || !name) return;
      try {
        await apiJson(fileHref(chatId, name), 'DELETE');
        staged = staged.filter((s) => s !== name);
        renderStaged();
        await refreshShared();
      } catch (err) { showError(err.message); }
    } else if (act === 'delete-chat') {
      e.preventDefault();
      await apiJson(`/api/chats/${encodeURIComponent(btn.dataset.id)}`, 'DELETE');
      location.href = chatId === btn.dataset.id ? '/' : location.pathname;
    } else if (act === 'new-in-folder') {
      e.preventDefault(); e.stopPropagation();
      const created = await apiJson('/api/chats', 'POST', { folder: btn.dataset.folder });
      location.href = `/c/${encodeURIComponent(created.id)}`;
    } else if (act === 'rename-folder') {
      e.preventDefault(); e.stopPropagation();
      const oldName = btn.dataset.folder;
      const newName = prompt('Rename folder:', oldName);
      if (newName && newName.trim() && newName.trim() !== oldName) {
        await apiJson('/api/folders', 'PATCH', { old_name: oldName, new_name: newName.trim() });
        location.reload();
      }
    } else if (act === 'delete-folder') {
      e.preventDefault(); e.stopPropagation();
      await apiJson('/api/folders', 'DELETE', { name: btn.dataset.folder });
      location.reload();
    }
  });

  if (input && form) {
    const resize = () => { input.style.height = 'auto'; input.style.height = `${Math.min(input.scrollHeight, 200)}px`; };
    input.addEventListener('input', resize);
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); form.requestSubmit(); } });
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (controller) return;
      const content = input.value.trim();
      if (!content && !staged.length) return;
      const attachments = [...staged];
      staged = [];
      renderStaged();
      input.value = '';
      resize();
      await runStream({ type: 'send', content, attachments }, null);
    });
  }
  scrollBottom();
})();
