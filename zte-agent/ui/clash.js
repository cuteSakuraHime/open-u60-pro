'use strict';
(() => {
  const route = 'network/clash', UI = OpenU60UI, API = OpenU60Console, M = OpenU60Clash;
  const {el,button,request,json,confirm,mutate,say} = UI;
  // Starting Mihomo, downloading a profile or testing a group takes far longer than a settings read.
  const SLOW = 150000, CONTROLLER = 40000;
  let current = null, groups = null, loading = false, attempted = false, lastLoadStarted = 0;
  let profileDirty = false, linkDirty = false, settingsDirty = false;
  const openGroups = new Set();

  // Status and the main switch.
  const statusPanel = UI.panel(route,'clash-status','Clash core','Mihomo runs on the router with a profile you import. It can serve as an explicit proxy only, or transparently carry the traffic of every device on this router.');
  statusPanel.classList.add('clash-panel');
  const hero = el('div','clash-hero'), glyph = el('div','clash-glyph'), heroCopy = el('div'), heroTitle = el('h3','','Status not read yet'), heroDetail = el('p',''), badge = el('span','clash-state','—');
  glyph.append(el('i'),el('i'),el('i')); heroCopy.append(el('span','eyebrow','MIHOMO CORE'),heroTitle,heroDetail); hero.append(glyph,heroCopy,badge);
  const facts = el('dl','clash-facts'), fact = {};
  for (const [key,label,mono] of [['routing','Routing'],['address','Explicit proxy',true],['profile','Profile'],['core','Core']]) {
    const pair = el('div'), value = el('dd',mono ? 'mono' : '','—'); pair.append(el('dt','',label),value); facts.append(pair); fact[key] = value;
  }
  const statusActions = el('div','form-actions'), refresh = button('Read current status',() => load()), toggle = button('Enable Clash',toggleCore,'primary'), restart = button('Restart core',restartCore);
  statusActions.append(refresh,toggle,restart); statusPanel.append(hero,facts,statusActions); const statusText = UI.status(statusPanel);

  // Policy groups, read from the running core.
  const groupsPanel = UI.panel(route,'clash-groups','Policy groups','Choose the outbound node of each selector group and measure latency through the running core. Automatic groups pick their own node.');
  const groupList = el('div','clash-group-list'), groupActions = el('div','form-actions'), reloadGroups = button('Read policy groups',() => loadGroups(true));
  groupActions.append(reloadGroups); groupsPanel.append(groupList,groupActions); const groupsText = UI.status(groupsPanel);

  // Transparent proxy settings.
  const proxyPanel = UI.panel(route,'clash-transparent','Transparent proxy','Redirect the traffic of devices on this router into Mihomo without configuring each device. The rules are removed the moment the core stops, so a broken profile cannot leave the hotspot offline.');
  const proxyForm = el('form','settings-form clash-settings-form'), proxyFields = el('fieldset','clash-options'), option = {};
  proxyFields.append(el('legend','sr-only','Transparent proxy'));
  for (const [key,label,help] of [
    ['transparent','Transparent proxy for every device','IPv4 TCP from Wi-Fi and USB clients is redirected to Mihomo. The router’s own traffic and local addresses stay direct.'],
    ['udp','Include UDP','Routes client UDP (QUIC, games, voice) through Mihomo’s TUN device. Turn it off if the profile’s nodes do not carry UDP.'],
    ['dns_hijack','Answer DNS with Mihomo','Client DNS queries on port 53 are answered by the profile’s dns section, or by a built-in resolver when it has none. Needed for domain rules to match.'],
    ['bypass_china','Keep mainland China addresses direct','Destinations in the downloaded mainland China IPv4 list skip Mihomo entirely, so its rules do not apply to them. The built-in resolver then answers with real addresses. A profile with its own fake-ip dns section hides those addresses, and only connections made by IP address are bypassed. The list is downloaded the first time this is turned on.']
  ]) {
    const row = el('label','clash-option'), input = el('input'), copy = el('span'); input.type = 'checkbox'; input.name = key; input.id = `clash-${key}`;
    copy.append(el('strong','',label),el('small','',help)); row.append(input,copy); proxyFields.append(row); option[key] = input;
  }
  const chinaRow = el('div','clash-inline'), chinaInfo = el('span','','Mainland China list: not downloaded'), chinaUpdate = button('Update list',updateChinaList); chinaRow.append(chinaInfo,chinaUpdate);
  const levelField = el('div','form-field clash-level'), levelLabel = el('label','','Core log level'), level = el('select'); level.id = 'clash-log-level'; level.name = 'log_level'; levelLabel.htmlFor = level.id;
  for (const name of M.LOG_LEVELS) level.add(new Option(name[0].toUpperCase() + name.slice(1),name));
  levelField.append(levelLabel,level,el('small','field-help','Info and Debug record every connection in the log below. Use them only while diagnosing.'));
  const proxyActions = el('div','form-actions'), applySettings = button('Apply settings',null,'primary'), discardSettings = button('Discard edits',() => { settingsDirty = false; drawSettings(); }); applySettings.type = 'submit';
  proxyActions.append(applySettings,discardSettings); proxyForm.append(proxyFields,chinaRow,levelField,proxyActions); proxyPanel.append(proxyForm);
  proxyPanel.append(el('p','panel-note','IPv6 is not proxied: only client DNS over IPv6 is intercepted, and the built-in resolver returns IPv4 answers so traffic matched by the rules stays on the proxied IPv4 path.'));
  const proxyText = UI.status(proxyPanel);

  // Profile: pasted YAML or a subscription link. Neither is ever read back from the router.
  const profilePanel = UI.panel(route,'clash-profile','Profile','Paste a Clash or Mihomo YAML profile. The router keeps its own listeners, controller and log level, checks the file with Mihomo, and only then replaces the running profile.');
  const profileForm = el('form','settings-form clash-profile-form'), profileFields = el('fieldset','form-grid'), profileField = el('div','form-field clash-config-field'), profileLabel = el('label','','Profile YAML'), textarea = el('textarea','mono');
  textarea.id = 'clash-config'; textarea.name = 'config'; textarea.rows = 12; textarea.autocomplete = 'off'; textarea.spellcheck = false; textarea.placeholder = 'proxies:\n  - {name: node-1, type: ss, server: …}\nproxy-groups:\n  - {name: Proxy, type: select, proxies: [node-1]}\nrules:\n  - MATCH,Proxy'; profileLabel.htmlFor = textarea.id;
  profileField.append(profileLabel,textarea,el('small','field-help','Profiles contain proxy credentials. They are stored on the router with owner-only permissions and are never returned by the API, so this box is empty when you come back. Use the HTTPS console when importing secrets.'));
  const runChoice = el('label','clash-option'), runAfter = el('input'), runCopy = el('span'); runAfter.type = 'checkbox'; runAfter.name = 'enabled'; runAfter.id = 'clash-run-after';
  runCopy.append(el('strong','','Run Clash with this profile'),el('small','','Unchecked, the profile is saved and Clash is left off.')); runChoice.append(runAfter,runCopy);
  const preview = el('div','clash-preview'); preview.hidden = true; profileFields.append(profileField,runChoice,preview);
  const profileActions = el('div','form-actions'), saveProfile = button('Validate & save',null,'primary'), clearProfile = button('Clear draft',() => { clearProfileDraft(); say(profileText,'Draft cleared.'); }); saveProfile.type = 'submit';
  profileActions.append(saveProfile,clearProfile); profileForm.append(profileFields,profileActions); profilePanel.append(profileForm); const profileText = UI.status(profilePanel);

  const linkPanel = UI.panel(route,'clash-subscription','Subscription','Download the profile from a provider link. The link usually carries your account token: it is stored on the router and only its host name is shown afterwards.');
  const linkForm = el('form','settings-form'), linkFields = el('fieldset','form-grid'), linkField = el('div','form-field clash-config-field'), linkLabel = el('label','','Subscription link'), link = el('input'), linkSaved = el('small','field-help','No subscription is saved.');
  link.type = 'url'; link.id = 'clash-subscription-url'; link.name = 'url'; link.autocomplete = 'off'; link.spellcheck = false; link.maxLength = 2048; link.placeholder = 'https://provider.example/subscribe?token=…'; linkLabel.htmlFor = link.id;
  linkField.append(linkLabel,link,linkSaved); linkFields.append(linkField);
  const linkActions = el('div','form-actions'), fetchLink = button('Download & apply',null,'primary'), forgetLink = button('Forget subscription',forgetSubscription,'danger'); fetchLink.type = 'submit';
  linkActions.append(fetchLink,forgetLink); linkForm.append(linkFields,linkActions); linkPanel.append(linkForm);
  linkPanel.append(el('p','panel-note','The link must return a Clash YAML profile. Many providers need a “clash” flag in the link for that. Updates are manual: nothing is downloaded in the background.'));
  const linkText = UI.status(linkPanel);

  // Core log, kept in the agent’s memory.
  const logPanel = UI.panel(route,'clash-log','Core log','The most recent output of the Mihomo process, kept in memory on the router and lost when the agent restarts.');
  const logView = el('pre','clash-log-view'), logActions = el('div','form-actions'), readLog = button('Read log',() => loadLog(true)), clearLog = button('Clear log',eraseLog);
  logView.tabIndex = 0; logView.setAttribute('aria-label','Mihomo log'); logActions.append(readLog,clearLog); logPanel.append(logView,logActions); const logText = UI.status(logPanel);

  const ready = () => !loading && !API.locked && API.connected;
  function draftSettings() { return {transparent:option.transparent.checked,udp:option.udp.checked,dns_hijack:option.dns_hijack.checked,bypass_china:option.bypass_china.checked,log_level:level.value}; }
  function drawSettings() {
    const saved = current?.settings;
    if (saved && !settingsDirty) { for (const key of ['transparent','udp','dns_hijack','bypass_china']) option[key].checked = !!saved[key]; level.value = saved.log_level; }
    proxyFields.disabled = level.disabled = !saved || loading;
    for (const key of ['udp','dns_hijack','bypass_china']) { option[key].disabled = !option.transparent.checked; option[key].closest('label').dataset.muted = String(!option.transparent.checked); }
    const changed = saved ? Object.keys(M.patch(saved,draftSettings())).length : 0;
    settingsDirty = settingsDirty && changed > 0; proxyPanel.dataset.dirty = String(settingsDirty);
    applySettings.disabled = !settingsDirty || !ready(); discardSettings.disabled = !settingsDirty;
    const list = current?.china_list;
    chinaInfo.textContent = list?.updated_at ? `Mainland China list: ${list.entries ?? '—'} networks · updated ${M.ago(list.updated_at,current.now)}` : 'Mainland China list: not downloaded';
    chinaUpdate.textContent = list?.updated_at ? 'Update list' : 'Download list'; chinaUpdate.disabled = !current || !ready();
  }
  function drawProfile() {
    const text = textarea.value; profileDirty = !!text.trim(); profilePanel.dataset.dirty = String(profileDirty);
    if (!profileDirty) runAfter.checked = !!current?.enabled;
    clearProfile.disabled = !profileDirty; preview.hidden = !profileDirty; let problem = '';
    if (profileDirty) {
      const info = M.inspect(text);
      problem = info.tooLarge ? 'This profile is larger than the 1 MiB the router accepts.' : info.encoded ? 'This looks like a base64 share-link list, not a Clash YAML profile. Ask the provider for the Clash format.' : !info.recognised ? 'No proxies, proxy-groups or rules section found. This is not a Clash profile.' : '';
      preview.dataset.error = String(!!problem);
      preview.replaceChildren(el('strong','',problem ? 'Cannot be imported' : 'Looks like a Clash profile'),el('span','',`${M.counts(info)}${info.has_dns ? ' · own DNS section' : ''}`),...(problem ? [el('small','',problem)] : []));
    } else preview.replaceChildren();
    saveProfile.disabled = !profileDirty || !!problem || !ready() || !current?.installed;
  }
  function clearProfileDraft() { textarea.value = ''; drawProfile(); }
  function drawLink() {
    const saved = current?.subscription; linkDirty = !!link.value.trim(); linkPanel.dataset.dirty = String(linkDirty);
    linkSaved.textContent = saved ? `Saved subscription: ${saved.host || 'unknown host'} · downloaded ${M.ago(saved.updated_at,current.now)}. Leave the box empty to download it again.` : 'No subscription is saved.';
    fetchLink.textContent = saved && !linkDirty ? 'Update subscription' : 'Download & apply';
    fetchLink.disabled = !(linkDirty || saved) || !ready() || !current?.installed; forgetLink.hidden = !saved; forgetLink.disabled = !ready();
  }
  function drawGroups() {
    reloadGroups.disabled = !current?.running || !ready();
    if (!current?.running) { groupList.replaceChildren(el('p','empty',current ? 'Policy groups are available while the core is running.' : 'Unlock the console to read policy groups.')); return; }
    if (!groups) { groupList.replaceChildren(el('p','empty','Policy groups have not been read yet.')); return; }
    if (!groups.length) { groupList.replaceChildren(el('p','empty','This profile defines no policy groups.')); return; }
    groupList.replaceChildren(...groups.map(group => {
      const card = el('article','clash-group'), heading = el('div','clash-group-heading'), copy = el('div'), chosen = group.members.find(member => member.name === group.now), chosenDelay = M.delay(chosen?.delay);
      const now = el('p','',group.now ? `Now: ${group.now}` : 'No node selected'); if (chosen && chosenDelay.tone !== 'none') { const mark = el('span','clash-delay',chosenDelay.text); mark.dataset.tone = chosenDelay.tone; now.append(' ',mark); }
      copy.append(el('h3','',group.name),now);
      const test = button('Test latency',() => testGroup(group.name)); test.disabled = !ready();
      heading.append(copy,el('span','tag',`${group.type || 'Group'} · ${group.members.length}`),test);
      const details = el('details','clash-members'), summary = el('summary','',group.selectable ? 'Choose a node' : 'Show nodes'), list = el('div','clash-member-list');
      details.open = openGroups.has(group.name); details.addEventListener('toggle',() => { if (details.open) openGroups.add(group.name); else openGroups.delete(group.name); });
      for (const member of group.members) {
        const delay = M.delay(member.delay), node = el(group.selectable ? 'button' : 'div','clash-member'), time = el('span','clash-delay',delay.text); time.dataset.tone = delay.tone;
        node.append(el('strong','',member.name),el('small','',member.type || '—'),time); node.dataset.current = String(member.name === group.now);
        if (group.selectable) { node.type = 'button'; node.setAttribute('aria-pressed',String(member.name === group.now)); node.disabled = !ready(); node.addEventListener('click',() => { if (member.name !== group.now) selectNode(group.name,member.name); }); }
        list.append(node);
      }
      details.append(summary,list); card.append(heading,details); return card;
    }));
  }
  function draw() {
    const data = current, state = M.phase(data);
    statusPanel.dataset.state = glyph.dataset.state = state.tone; badge.textContent = state.badge; heroTitle.textContent = state.title; heroDetail.textContent = state.detail;
    fact.routing.textContent = !data?.running ? '—' : data.transparent_active ? `Transparent${data.settings?.udp ? ' · TCP + UDP' : ' · TCP'}` : 'Explicit proxy only';
    fact.address.textContent = data?.proxy_address || '—'; fact.profile.textContent = data?.configured ? M.counts(data.summary) : '—';
    fact.core.textContent = data ? `${M.version(data.version)}${data.running ? ` · up ${M.uptime(data.uptime)}` : ''}` : '—';
    const enabled = !!data?.enabled;
    toggle.textContent = enabled ? 'Disable Clash' : 'Enable Clash'; toggle.className = `button ${enabled ? 'danger' : 'primary'}`;
    toggle.disabled = !ready() || !data || (!enabled && (!data.installed || !data.configured));
    restart.disabled = !ready() || !enabled; refresh.disabled = loading || API.locked || !API.connected;
    readLog.disabled = clearLog.disabled = !data || !ready();
    drawSettings(); drawProfile(); drawLink(); drawGroups();
  }

  async function readStatus() { current = await request('/api/vpn/clash'); if (!current.running) groups = null; }
  async function loadGroups(announce = false) {
    if (!current?.running || API.locked) return;
    if (announce) say(groupsText,'Reading policy groups…');
    try { groups = (await request('/api/vpn/clash/proxies',{timeoutMs:CONTROLLER})).groups || []; if (announce) say(groupsText,`${groups.length} policy group${groups.length === 1 ? '' : 's'} read from the core.`); else settle(groupsText); }
    catch (error) { say(groupsText,`Policy groups unavailable. ${error.message}`,true); }
    drawGroups();
  }
  // A background read leaves the result of the last action in place and only retires an old failure.
  function settle(node) { if (node.dataset.error === 'true') say(node,''); }
  function showLog(text) { const lines = String(text || '').trimEnd(); logView.textContent = lines || 'The log is empty.'; logView.dataset.empty = String(!lines); logView.scrollTop = logView.scrollHeight; }
  async function loadLog(announce = false) {
    if (API.locked) return;
    try { showLog((await request('/api/vpn/clash/log')).text); if (announce) say(logText,'Log read.'); else settle(logText); }
    catch (error) { say(logText,`Log unavailable. ${error.message}`,true); }
  }
  // `full` also reads the log and the policy groups (a call into the core); a
  // focus refresh reads the groups only when a core has started since.
  async function load(full = true) {
    if (loading || API.locked || !API.connected) return;
    loading = true; attempted = true; lastLoadStarted = Date.now(); draw(); let failure = '';
    try { await readStatus(); } catch (error) { failure = `Clash status unavailable. ${error.message}`; }
    loading = false; draw(); say(statusText,failure,!!failure);
    if (failure) return;
    if (full) await Promise.all([loadLog(),loadGroups()]);
    else if (current.running && !groups) await loadGroups();
  }
  // One change at a time. A reply from the router is shown as it is; no reply
  // at all means the outcome is unknown, which `mutate` reports.
  async function act(node,busy,run,done) {
    await mutate(node,async live => {
      loading = true; draw(); say(node,busy); let failure = '', result;
      try { result = await run(); }
      catch (error) { if (error.status === undefined) { loading = false; if (live()) draw(); throw error; } failure = error.message; }
      if (!live()) { loading = false; return; }
      if (failure) { try { await readStatus(); } catch { /* The failure itself is what the user needs to read. */ } }
      loading = false; if (!live()) return;
      if (failure) say(node,failure,true); else { done(result); if (node.textContent === busy) say(node,'Done.'); }
      draw();
      // Group actions already return the groups, and clearing the log returns it empty.
      if (!current?.running) drawGroups(); else if (node !== groupsText) loadGroups();
      if (node !== logText) loadLog();
    });
  }
  function adopt(data) { current = data; if (!data.running) groups = null; say(statusText,''); }
  async function toggleCore() {
    if (!current || loading) return; const disabling = !!current.enabled;
    const text = disabling ? 'Mihomo stops and the transparent proxy rules are removed at once. Devices go back to the direct path; connections that were open through the proxy are dropped. The profile stays saved.'
      : current.settings?.transparent ? 'Mihomo starts with the saved profile and the transparent proxy is installed once it answers. Devices on this router then follow the profile’s rules.'
        : `Mihomo starts with the saved profile as an explicit proxy at ${current.proxy_address}. Other traffic is not touched.`;
    if (!await confirm(disabling ? 'Disable Clash?' : 'Enable Clash?',text,{label:disabling ? 'Disable Clash' : 'Enable Clash',destructive:disabling})) return;
    await act(statusText,disabling ? 'Stopping the core…' : 'Starting the core and waiting for it to answer…',() => request(`/api/vpn/clash/${disabling ? 'disable' : 'enable'}`,{...json('POST',{}),timeoutMs:SLOW}),data => { adopt(data); say(statusText,disabling ? 'Clash is off.' : data.last_error ? `Core started. ${data.last_error}` : 'Core started.',!disabling && !!data.last_error); });
  }
  async function restartCore() {
    if (!current?.enabled || loading) return;
    if (!await confirm('Restart the Clash core?','Mihomo restarts with the saved profile. Connections through the proxy are dropped and devices use the direct path for a few seconds.',{label:'Restart core'})) return;
    await act(statusText,'Restarting the core…',() => request('/api/vpn/clash/restart',{...json('POST',{}),timeoutMs:SLOW}),data => { adopt(data); say(statusText,data.last_error ? `Core restarted. ${data.last_error}` : 'Core restarted.',!!data.last_error); });
  }
  async function selectNode(group,proxy) {
    await act(groupsText,`Switching ${group}…`,() => request('/api/vpn/clash/proxies',{...json('PUT',{group,proxy}),timeoutMs:CONTROLLER}),data => { groups = data.groups || []; say(groupsText,`${group} now uses ${proxy}.`); });
  }
  async function testGroup(group) {
    await act(groupsText,`Testing ${group}… this can take a few seconds.`,() => request('/api/vpn/clash/proxies/test',{...json('POST',{group}),timeoutMs:CONTROLLER}),data => { groups = data.groups || []; openGroups.add(group); say(groupsText,`Latency measured for ${group}.`); });
  }
  async function updateChinaList() {
    await act(proxyText,'Downloading the mainland China address list…',() => request('/api/vpn/clash/china-list',{...json('POST',{}),timeoutMs:SLOW}),data => { adopt(data); say(proxyText,`Mainland China list updated: ${data.china_list?.entries ?? '—'} networks.`); });
  }
  async function forgetSubscription() {
    if (!current?.subscription || !await confirm('Forget the subscription link?','The stored link is deleted from the router. The profile downloaded from it stays in use.',{label:'Forget link',destructive:true})) return;
    await act(linkText,'Removing the stored link…',() => request('/api/vpn/clash/subscription',{method:'DELETE'}),data => { adopt(data); say(linkText,'Subscription link removed.'); });
  }
  async function eraseLog() {
    await act(logText,'Clearing the log…',() => request('/api/vpn/clash/log',{method:'DELETE'}),() => { showLog(''); say(logText,'Log cleared.'); });
  }

  proxyForm.addEventListener('change',() => { settingsDirty = true; drawSettings(); if (settingsDirty) say(proxyText,'Unsaved changes.'); else say(proxyText,''); });
  proxyForm.addEventListener('submit',async event => {
    event.preventDefault(); if (!current || !settingsDirty || loading) return;
    const body = M.patch(current.settings,draftSettings()); if (!Object.keys(body).length) return;
    const turningOn = body.transparent === true, turningOff = body.transparent === false, running = !!current.running;
    const text = turningOn ? `Every device on this router will go through Mihomo${running ? ' as soon as you apply this' : ' whenever Clash is enabled'}. If the profile’s nodes are unreachable, destinations its rules send to a proxy stop loading until you turn this off. This console stays reachable at the router’s address.`
      : turningOff ? 'The redirect rules are removed and devices return to the direct path. Mihomo keeps running as an explicit proxy.'
        : running ? 'Mihomo may restart to apply this; connections through the proxy are dropped for a few seconds. If the new settings fail, the previous ones are restored.' : '';
    if (text && !await confirm(turningOn ? 'Turn on the transparent proxy?' : turningOff ? 'Turn off the transparent proxy?' : 'Apply Clash settings?',text,{label:'Apply settings'})) return;
    await act(proxyText,'Applying settings…',() => request('/api/vpn/clash/settings',{...json('PUT',body),timeoutMs:SLOW}),data => { settingsDirty = false; adopt(data); say(proxyText,data.last_error ? `Settings saved. ${data.last_error}` : running ? 'Settings applied.' : 'Settings saved. They take effect when Clash is enabled.',!!data.last_error); });
  });
  textarea.addEventListener('input',() => { drawProfile(); say(profileText,profileDirty ? 'The draft stays only in this tab until you save it.' : ''); });
  profileForm.addEventListener('submit',async event => {
    event.preventDefault(); if (!profileDirty || loading || saveProfile.disabled) return;
    const source = textarea.value, enabled = runAfter.checked, info = M.inspect(source);
    const text = `${enabled ? 'Check this profile with Mihomo, save it and run Clash with it' : current?.enabled ? 'Check this profile with Mihomo, save it and turn Clash off' : 'Check this profile with Mihomo and save it without starting Clash'} (${M.counts(info)})? The saved profile is replaced only if the check passes.`;
    if (!await confirm('Save Clash profile?',text,{label:enabled ? 'Save & run' : 'Save profile',destructive:!enabled && !!current?.enabled})) return;
    await act(profileText,'Checking the profile with Mihomo…',() => request('/api/vpn/clash',{...json('PUT',{config:source,enabled}),timeoutMs:SLOW}),data => { adopt(data); clearProfileDraft(); say(profileText,enabled ? 'Profile saved and Clash is running with it.' : 'Profile checked and saved.'); });
  });
  link.addEventListener('input',() => { drawLink(); say(linkText,''); });
  linkForm.addEventListener('submit',async event => {
    event.preventDefault(); if (loading || fetchLink.disabled) return;
    const typed = link.value.trim(), url = M.subscriptionUrl(typed);
    if (typed && !url) { say(linkText,'Enter a link that starts with http:// or https://.',true); return; }
    const host = typed ? (() => { try { return new URL(url).host; } catch { return 'the provider'; } })() : current?.subscription?.host || 'the provider';
    if (!await confirm(typed ? 'Download this subscription?' : 'Update the subscription?',`The router downloads the profile from ${host} over its own internet connection, checks it with Mihomo and replaces the saved profile if the check passes. ${current?.enabled ? 'Clash restarts with the new profile.' : 'Clash stays off until you enable it.'}`,{label:typed ? 'Download & apply' : 'Update subscription'})) return;
    await act(linkText,'Downloading and checking the profile…',() => request('/api/vpn/clash/subscription',{...json('POST',typed ? {url} : {}),timeoutMs:SLOW}),data => { adopt(data); link.value = ''; say(linkText,`Profile downloaded: ${M.counts(data.summary)}.`); });
  });

  function refreshVisible() { if (document.hidden || loading || UI.saving || API.locked || !API.connected || OpenU60Navigation.current.key !== route || Date.now() - lastLoadStarted < 1000) return; load(false); }
  window.addEventListener('focus',refreshVisible);
  window.addEventListener('pageshow',refreshVisible);
  document.addEventListener('visibilitychange',() => { if (!document.hidden) refreshVisible(); });
  UI.track({route,get loaded() { return attempted; },get dirty() { return profileDirty || linkDirty || settingsDirty; },load:() => load(true),refresh:() => load(true),clear() {
    current = null; groups = null; attempted = false; loading = false; settingsDirty = false; openGroups.clear(); textarea.value = ''; link.value = ''; showLog('');
    for (const node of [statusText,groupsText,proxyText,profileText,linkText,logText]) say(node,''); draw();
  }});
  showLog(''); draw();
})();
