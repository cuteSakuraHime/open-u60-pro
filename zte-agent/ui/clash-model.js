'use strict';
((globalThis) => {
  // Mirrors the agent's limits so a draft is refused before it is uploaded.
  const MAX_CONFIG = 1024 * 1024, MAX_URL = 2048;
  const LOG_LEVELS = ['silent','error','warning','info','debug'];
  const SETTINGS = ['transparent','udp','dns_hijack','bypass_china','log_level'];
  const RECOGNISED = new Set(['proxies','proxy-providers','proxy-groups','rules','rule-providers','mixed-port','port','mode']);
  const plural = (count,one,many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
  function topLevelKey(line) {
    if (!line || line[0] === '.') return null;
    const match = /^(['"]?)([A-Za-z0-9_.-]+)\1:(?:[ \t]|$)/.exec(line);
    return match ? match[2] : null;
  }
  const content = line => { const trimmed = line.trimStart(); return trimmed !== '' && !trimmed.startsWith('#'); };
  const indent = line => line.length - line.trimStart().length;
  function entries(lines,sequence) {
    const items = lines.filter(content);
    if (!items.length) return 0;
    const depth = indent(items[0]);
    return items.filter(line => indent(line) === depth && (!sequence || line.trimStart().startsWith('-'))).length;
  }
  // A local preview of a pasted profile. The router repeats every check and
  // has Mihomo itself validate the file before anything is replaced.
  function inspect(text) {
    const source = String(text || '').replace(/^\uFEFF/,'');
    const result = {bytes:new TextEncoder().encode(source).length,recognised:false,encoded:false,tooLarge:false,proxies:0,proxy_providers:0,proxy_groups:0,rules:0,has_dns:false};
    const blocks = [];
    for (const raw of source.split('\n')) {
      const line = raw.endsWith('\r') ? raw.slice(0,-1) : raw, key = topLevelKey(line);
      if (key === null && blocks.length) blocks.at(-1).lines.push(line); else blocks.push({key,lines:[]});
    }
    for (const {key,lines} of blocks) {
      if (RECOGNISED.has(key)) result.recognised = true;
      if (key === 'proxies') result.proxies = entries(lines,true);
      else if (key === 'proxy-groups') result.proxy_groups = entries(lines,true);
      else if (key === 'rules') result.rules = entries(lines,true);
      else if (key === 'proxy-providers') result.proxy_providers = entries(lines,false);
      else if (key === 'dns') result.has_dns = lines.some(content);
    }
    result.tooLarge = result.bytes > MAX_CONFIG;
    // A share-link list (base64) instead of a Clash YAML profile.
    result.encoded = !result.recognised && /^[A-Za-z0-9+/=_-]{32,}$/.test(source.replace(/\s+/g,''));
    return result;
  }
  function counts(summary) {
    if (!summary) return '—';
    const parts = [plural(Number(summary.proxies) || 0,'proxy','proxies')];
    if (Number(summary.proxy_providers)) parts.push(plural(Number(summary.proxy_providers),'provider'));
    parts.push(plural(Number(summary.proxy_groups) || 0,'group'),plural(Number(summary.rules) || 0,'rule'));
    return parts.join(' · ');
  }
  function version(text) {
    const source = String(text || '').trim();
    if (!source) return '—';
    return /\bv\d+\.\d+\.\d+[\w.-]*/.exec(source)?.[0] || source.slice(0,40);
  }
  function duration(seconds) {
    const total = Math.max(0,Math.floor(Number(seconds) || 0));
    if (total < 60) return `${total}s`;
    if (total < 3600) return `${Math.floor(total / 60)}m`;
    if (total < 86400) return `${Math.floor(total / 3600)}h ${Math.floor(total % 3600 / 60)}m`;
    return `${Math.floor(total / 86400)}d ${Math.floor(total % 86400 / 3600)}h`;
  }
  const known = seconds => Number.isFinite(Number(seconds)) && Number(seconds) > 0;
  // `seconds` and `now` are both the router's clock (`now` comes with every
  // status reply). The browser's clock is never mixed in: the two can differ
  // by hours, and the router's may not be set at all.
  const uptime = seconds => Number.isFinite(Number(seconds)) && seconds !== null && seconds !== undefined ? duration(seconds) : '—';
  const ago = (seconds,now) => !known(seconds) ? 'never' : known(now) ? `${duration(Number(now) - Number(seconds))} ago` : 'at an unknown time';
  function phase(data) {
    if (!data) return {tone:'idle',badge:'—',title:'Status not read yet',detail:'Unlock the console to read the Clash status.'};
    const problem = data.last_error ? ` ${data.last_error}` : '';
    if (!data.installed) return {tone:'error',badge:'CORE MISSING',title:'Mihomo core is not installed',detail:'Copy the Mihomo arm64 binary to /data/open-u60/clash/mihomo and make it executable.'};
    if (!data.configured) return {tone:'idle',badge:'NO PROFILE',title:'No profile yet',detail:'Paste a Clash profile or add a subscription to begin.'};
    if (data.running && data.transparent_active) return {tone:'active',badge:'TRANSPARENT',title:'Transparent proxy active',detail:'Traffic from devices on this router goes through Mihomo and follows the rules of the profile.'};
    if (data.running && data.settings?.transparent) return {tone:'error',badge:'NEEDS ATTENTION',title:'Core running, transparent proxy not installed',detail:`Devices are on the direct path. Only the explicit proxy at ${data.proxy_address} is available.${problem}`};
    if (data.running) return {tone:'active',badge:'RUNNING',title:'Core running as an explicit proxy',detail:`Devices use it only when their HTTP or SOCKS5 proxy is set to ${data.proxy_address}. Turn on the transparent proxy to cover every device.`};
    if (data.enabled) return {tone:'error',badge:'NEEDS ATTENTION',title:'Enabled, but the core is not running',detail:`Devices are on the direct path.${problem || ' Read the log for the reason, then restart the core.'}`};
    return {tone:'idle',badge:'OFF',title:'Profile saved',detail:`The saved profile is ready when you enable Clash.${problem}`};
  }
  function delay(value) {
    if (value === null || value === undefined || !Number.isFinite(Number(value))) return {text:'—',tone:'none'};
    const ms = Number(value);
    if (ms <= 0) return {text:'Timeout',tone:'bad'};
    return {text:`${ms} ms`,tone:ms < 250 ? 'good' : ms < 700 ? 'fair' : 'bad'};
  }
  // Only the settings that differ, as the PUT body.
  function patch(baseline,draft) {
    const changed = {};
    for (const key of SETTINGS) if (draft?.[key] !== undefined && draft[key] !== baseline?.[key]) changed[key] = draft[key];
    return changed;
  }
  function subscriptionUrl(text) {
    const url = String(text || '').trim();
    return /^https?:\/\/[^\s\x00-\x1f\x7f]+$/.test(url) && url.length <= MAX_URL ? url : '';
  }
  const api = {MAX_CONFIG,LOG_LEVELS,inspect,counts,version,duration,uptime,ago,phase,delay,patch,subscriptionUrl};
  if (typeof module === 'object' && module.exports) module.exports = api; else globalThis.OpenU60Clash = api;
})(globalThis);
