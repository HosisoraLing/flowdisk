'use strict';
const $ = id => document.getElementById(id);
let currentPath = '/', page = 1, total = 0, entries = [], roots = [], selected = new Set(), view = 'browse', timer, busy = false;
let toastTimer, accounts = [], loginSession = null, loginPoll, loginChecking = false, loginRevision = 0, pollCount = 0;
const labels = {queued:'等待下载',downloading:'正在下载',pausing:'正在暂停',paused:'已暂停',failed:'下载失败',completed:'已完成',seeding:'正在做种',metadata:'读取种子信息',selecting:'等待选择文件'};
function text(tag, value, cls) { const el = document.createElement(tag); el.textContent = value; if(cls) el.className = cls; return el; }
function button(label, action, cls) { const el = text('button', label, cls); el.type = 'button'; el.onclick = action; return el; }
function toast(message) { $('toast').textContent = message; $('toast').hidden = false; clearTimeout(toastTimer); toastTimer = setTimeout(() => $('toast').hidden = true, 5000); }
function bytes(n) { if(!Number.isFinite(n)) return '—'; if(n === 0) return '0 B'; const u = ['B','KB','MB','GB','TB']; const i = Math.min(4,Math.floor(Math.log(n)/Math.log(1024))); return `${(n / 1024 ** i).toFixed(i ? 1 : 0)} ${u[i]}`; }
async function api(path, options = {}) {
  const res = await fetch('/api' + path, {headers:{'Content-Type':'application/json'}, ...options});
  const data = await res.json();
  if(res.status === 401) { loginView(); throw Error(data.error || '请先登录'); }
  if(!res.ok) throw Error(data.error || `请求失败 ${res.status}`);
  return data;
}
function loginView() { clearInterval(timer); $('login').hidden = false; $('workspace').hidden = true; }
async function initialize() {
  try {
    const status = await api('/status');
    $('login').hidden = true; $('workspace').hidden = false;
    $('connection').textContent = status.gateway_connected ? '网盘网关已连接' : '网盘网关待配置';
    $('thread-setting').textContent = `每文件最多 ${status.threads} 连接`;
    if(!status.gateway_connected) toast(status.gateway_error || '请先配置 OpenList 网盘');
    await loadAccounts(); await loadSmb(); await loadRoots(); await browse('/',1); await loadJobs(); await loadShares();
    clearInterval(timer); timer = setInterval(() => { loadJobs().catch(e => toast(e.message)); if(view==='statistics'&&typeof loadStatistics==='function')loadStatistics().catch(()=>{}); if(++pollCount % 3 === 0) loadShares().catch(()=>{}); },1500);
  } catch(e) { if(!$('workspace').hidden) toast(e.message); else { loginView(); $('login-error').textContent = e.message === '请先登录' ? '' : e.message; } }
}
$('login-form').onsubmit = async event => {
  event.preventDefault();
  try { await api('/session',{method:'POST',body:JSON.stringify({token:$('token').value})}); $('token').value = ''; await initialize(); } catch(e) { $('login-error').textContent = e.message; }
};
$('logout').onclick = async () => { await api('/logout',{method:'POST'}); loginView(); };
function setView(next) {
  view = next;
  for(const name of ['browse','tasks','local','accounts','links','statistics']) { $(name+'-view').hidden = next !== name; $(name+'-tab').classList.toggle('active',next===name); }
  if(next==='local') loadLocal().catch(e=>toast(e.message));
  if(next==='statistics'&&typeof loadStatistics==='function')loadStatistics().catch(e=>toast(e.message));
  if(next==='links'&&typeof loadTools==='function')loadTools().catch(e=>toast(e.message));
  if(next==='accounts') loadAccounts().catch(e=>toast(e.message));
}
for(const name of ['browse','tasks','local','accounts','links','statistics']) $(name+'-tab').onclick=()=>setView(name);
function join(name) { return currentPath.replace(/\/$/,'') + '/' + name; }
async function loadRoots() { try { const result = await api('/browse?path=%2F'); roots = result.content || []; } catch { roots = []; } renderDrives(); }
function renderDrives() {
  $('drives').replaceChildren();
  const icons={baidu:'B',quark:'Q','115':'115',thunder:'↯',onedrive:'☁',google:'△'};
  for(const account of accounts) {
    const el = button('',async () => { if(account.connected) {setView('browse');await browse(account.mount_path,1);} else openAccount(account.id); },'drive');
    el.dataset.provider=account.id;el.append(text('span',icons[account.id],'drive-icon')); const info=document.createElement('span');info.append(text('strong',account.name),text('small',account.connected?'已连接 · 点击浏览':'点击登录'));el.append(info);el.classList.toggle('chosen',account.connected&&currentPath.startsWith(account.mount_path+'/'));$('drives').append(el);
  }
}
async function browse(path = currentPath, nextPage = page, refresh = false) {
  if(busy) return; busy = true; $('refresh').disabled = true;
  try {
    const result = await api('/browse?' + new URLSearchParams({path,page:nextPage,refresh}));
    currentPath = path; page = nextPage; entries = result.content || []; total = result.total || 0;
    selected.clear(); $('search').value = ''; $('files').replaceChildren(); $('empty').hidden = true;
    if(path === '/') { roots = entries; }
    renderDrives(); renderBreadcrumbs(); renderFiles();
  } catch(e) {
    toast(e.message);
    if(!entries.length) {
      $('empty').replaceChildren(text('p','尚未连接网盘，请点击上方的网盘卡片登录。'));
      const link = text('a','高级网盘配置 ↗'); const url = new URL(location.href); url.port = '5244'; url.pathname = '/@manage/storages'; url.search = ''; url.hash = ''; link.href = url.href; link.target = '_blank'; link.rel = 'noopener'; $('empty').append(link); $('empty').hidden = false;
    }
  } finally { busy = false; $('refresh').disabled = false; }
}
function renderBreadcrumbs() {
  $('breadcrumbs').replaceChildren(button('全部网盘',() => browse('/',1)));
  let prefix = '';
  for(const name of currentPath.split('/').filter(Boolean)) { prefix += '/' + name; const target = prefix; $('breadcrumbs').append(text('span','/'),button(name,() => browse(target,1))); }
}
function renderFiles() {
  $('files').replaceChildren();
  const filter = $('search').value.toLocaleLowerCase();
  const visible = entries.filter(e => e.name.toLocaleLowerCase().includes(filter));
  for(const file of visible) {
    const tr = document.createElement('tr'); const checkCell = text('td','','check');
    const checkbox = document.createElement('input'); checkbox.type = 'checkbox'; checkbox.checked = selected.has(file.name); checkbox.setAttribute('aria-label','选择 '+file.name); checkbox.onchange = () => { checkbox.checked ? selected.add(file.name) : selected.delete(file.name); updateSelection(); }; checkCell.append(checkbox);
    const nameCell = document.createElement('td'); const name = text('div','','filename'); name.append(text('span',file.is_dir ? '▰' : '↓','file-icon'+(file.is_dir?' folder':'')));
    name.append(file.is_dir ? button(file.name,() => browse(join(file.name),1)) : text('span',file.name)); nameCell.append(name);
    const date = new Date(file.modified); const action = text('td','','action');
    if(file.is_dir) action.append(button('→',() => browse(join(file.name),1))); else { action.append(button('预览',()=>previewCloud(join(file.name),file.name)),button('↓',() => enqueue(join(file.name)))); }
    tr.append(checkCell,nameCell,text('td',file.is_dir?'—':bytes(file.size),'size'),text('td',Number.isNaN(+date)?'—':date.toLocaleDateString('zh-CN'),'modified'),action); $('files').append(tr);
  }
  $('empty').hidden = visible.length > 0; if(!visible.length) $('empty').textContent = filter ? '当前页没有匹配的文件。' : currentPath === '/' ? '还没有连接网盘，点击上方卡片即可登录。' : '这个文件夹还没有内容。';
  $('file-count').textContent = `${total} 项内容${filter ? ` · 当前页匹配 ${visible.length} 项` : ''}`; $('page').textContent = `${page} / ${Math.max(1,Math.ceil(total/100))}`;
  $('previous').disabled = page <= 1; $('next').disabled = page * 100 >= total; updateSelection();
}
function updateSelection() { $('download-selected').disabled = !selected.size; $('rename-cloud').disabled=selected.size!==1||currentPath==='/'; $('move-cloud').disabled=$('delete-cloud').disabled=!selected.size||currentPath==='/'; $('upload-cloud').disabled=$('mkdir').disabled=currentPath==='/'; $('selected-count').textContent = selected.size || ''; const visible = entries.filter(e=>e.name.toLocaleLowerCase().includes($('search').value.toLocaleLowerCase())); const count = visible.filter(e=>selected.has(e.name)).length; $('select-all').checked = !!visible.length && count===visible.length; $('select-all').indeterminate = count>0&&count<visible.length; }
$('select-all').onchange = () => { entries.filter(e=>e.name.toLocaleLowerCase().includes($('search').value.toLocaleLowerCase())).forEach(e => $('select-all').checked ? selected.add(e.name) : selected.delete(e.name)); renderFiles(); };
$('search').oninput = renderFiles;
$('refresh').onclick = async () => { await loadRoots(); await browse(currentPath,page,true); };
$('previous').onclick = () => browse(currentPath,page-1); $('next').onclick = () => browse(currentPath,page+1);
async function enqueue(path, quiet = false) { try { await api('/jobs',{method:'POST',body:JSON.stringify({path})}); if(!quiet) toast('已添加到下载任务'); await loadJobs(); return true; } catch(e) { toast(e.message); return false; } }
$('download-selected').onclick=async()=>{try{const result=await api('/batch-download',{method:'POST',body:JSON.stringify({paths:[...selected].map(join)})});selected.clear();renderFiles();await loadJobs();toast(`已添加 ${result.jobs.length} 个下载任务`);}catch(e){toast(e.message);}};
let transferIds=new Set();
async function jobAction(id,action) { try { await api(`/${transferIds.has(id)?'transfers':'jobs'}/${id}${action ? '/'+action : ''}`,{method:action?'POST':'DELETE'}); await loadJobs(); } catch(e) { toast(e.message); } }
function speedChart(job) {
  const box=text('div','','speed-chart steam-chart');
  const points=job.speed_history||[];
  if(!points.length) { box.append(text('small','开始下载后显示速度曲线；旧任务没有历史统计。'));return box; }
  const ns='http://www.w3.org/2000/svg';
  const svg=(tag,attrs)=>{const node=document.createElementNS(ns,tag);for(const [k,v]of Object.entries(attrs))node.setAttribute(k,v);return node;};
  const graph=svg('svg',{viewBox:'0 0 600 100',preserveAspectRatio:'none',role:'img','aria-label':'下载速度折线图'});
  const max=Math.max(1,...points.map(p=>p.speed));
  const from=points[0].at_ms,to=Math.max(from+1000,points.at(-1).at_ms);
  const coords=points.map(p=>[8+(p.at_ms-from)/(to-from)*584,92-p.speed/max*80]);
  const stepped=coords.flatMap((p,i)=>i?[[p[0],coords[i-1][1]],p]:[p]);
  for(const y of [12,52,92]) graph.append(svg('line',{x1:8,y1:y,x2:592,y2:y,class:'speed-grid'}));
  graph.append(svg('polygon',{points:[[coords[0][0],92],...stepped,[coords.at(-1)[0],92]].map(p=>p.join(',')).join(' '),class:'speed-area'}));
  graph.append(svg('polyline',{points:stepped.map(p=>p.join(',')).join(' '),class:'speed-line'}));
  const heading=text('div','','steam-chart-heading');heading.append(text('strong','网络使用情况'),text('small',bytes(max)+'/s'));box.append(heading);
  const caption=text('div','','speed-caption');
  const time=t=>new Date(t).toLocaleTimeString([],{hour12:false});
  caption.append(text('small',time(from)),text('small','下载速度 · 最近 180 个采样'),text('small',time(points.at(-1).at_ms)));
  box.append(graph,caption);return box;
}
let collapsedGroups=new Set();
try{const saved=JSON.parse(localStorage.getItem('flowdisk-collapsed-groups')||'[]');if(Array.isArray(saved))collapsedGroups=new Set(saved.filter(v=>typeof v==='string'));}catch{}
async function loadJobs() {
  const [native,external]=await Promise.all([api('/jobs'),api('/transfers')]);const jobs=[...native,...external].sort((a,b)=>b.created_at-a.created_at);transferIds=new Set(external.map(j=>j.id)); const active = jobs.filter(j=>['downloading','queued','pausing','seeding','metadata'].includes(j.status));
  $('task-count').textContent = jobs.length; $('total-speed').textContent = bytes(jobs.reduce((n,j)=>n+j.speed,0))+'/s'; $('active-count').textContent = active.length; $('completed-count').textContent = jobs.filter(j=>j.status==='completed').length;
  if(view !== 'tasks' && !$('tasks').children.length && !jobs.length) return;
  $('tasks').replaceChildren(); $('no-tasks').hidden = !!jobs.length;
  const destinations=new Map(),groups=new Map();
  for(const job of jobs){const key=job.group_id||('single:'+job.id);if(!groups.has(key))groups.set(key,[]);groups.get(key).push(job);}
  for(const [key,members]of groups){
    if(key.startsWith('single:')){destinations.set(key,$('tasks'));continue;}
    const group=document.createElement('details');group.className='download-group';group.open=!collapsedGroups.has(key);
    const summary=document.createElement('summary'),heading=text('div','','group-heading'),total=members.reduce((s,j)=>s+(j.size||0),0),done=members.reduce((s,j)=>s+j.downloaded,0),completed=members.filter(j=>j.status==='completed').length,failed=members.filter(j=>j.status==='failed').length,rate=members.reduce((s,j)=>s+j.speed,0);
    heading.append(text('strong',members[0].group_name||'下载批次'),text('small',`${completed} / ${members.length} 个已完成${failed?' · '+failed+' 个失败':''}`));
    const stats=text('div','','group-stats');stats.append(text('b',`${total?Math.min(100,done/total*100).toFixed(1):0}%`),text('small',bytes(done)+' / '+bytes(total)+(rate?' · '+bytes(rate)+'/s':'')));
    summary.append(heading,stats);summary.onclick=()=>{group.open?collapsedGroups.add(key):collapsedGroups.delete(key);try{localStorage.setItem('flowdisk-collapsed-groups',JSON.stringify([...collapsedGroups]));}catch{}};
    const content=text('div','','group-content');group.append(summary,content);$('tasks').append(group);destinations.set(key,content);
  }
  for(const job of jobs) {
    const task = text('article','','task'); const top = text('div','','task-top'); const title = text('div','','task-title'); title.append(text('strong',job.name),text('small',job.path)); if(job.passwords?.length&&typeof passwordNotes==='function')title.append(passwordNotes(job.passwords)); const actions = text('div','','task-actions');
    if(['downloading','queued','seeding','metadata'].includes(job.status)) actions.append(button('暂停',()=>jobAction(job.id,'pause')));
    if(['paused','failed'].includes(job.status)) actions.append(button(job.status==='failed'?'重试':'继续',()=>jobAction(job.id,'resume')));
    if(job.engine==='aria2'){appendTransferActions(job,actions);}else if(job.status==='completed') { const link = text('a','保存到设备'); link.href = `/api/jobs/${job.id}/file`; link.setAttribute('download',job.name); actions.append(link); }
    if(!['downloading','queued','pausing','seeding','metadata'].includes(job.status)) actions.append(button('移除记录',()=>jobAction(job.id,'')));
    top.append(title,actions); const progress = text('div','','progress'); const fill = document.createElement('div'); const pct = job.status==='completed'?100:job.size ? Math.min(100,100*job.downloaded/job.size) : 0; fill.style.width = pct+'%'; progress.append(fill);
    const info = text('div','','task-info'); info.append(text('span',`${labels[job.status]||job.status} · ${bytes(job.downloaded)} / ${bytes(job.size)} · ${pct.toFixed(1)}%`),text('span',job.status==='downloading'?`${bytes(job.speed)}/s · 最多 ${job.connections} 连接`:'')); task.append(top,progress,info);
    const metrics=text('div','','speed-metrics');
    for(const [label,value]of [['实时速度',job.speed],['峰值速度',job.peak_speed],['平均速度',job.average_speed]]) {
      const metric=text('div');metric.append(text('small',label),text('strong',value==null||(!job.active_ms&&label!=='实时速度')?'暂无统计':bytes(value)+'/s'));metrics.append(metric);
    }
    const eta=text('div','','eta');
    if(job.status==='completed') eta.textContent='下载已完成';
    else if(job.status==='paused'||job.status==='pausing') eta.textContent='已暂停 · 继续后估算完成时间';
    else if(job.status==='queued') eta.textContent='等待开始 · 完成时间待估算';
    else if(job.status==='downloading') {
      const recent=(job.speed_history||[]).filter(p=>p.at_ms>=Date.now()-12000).slice(-10);
      const rate=recent.length?recent.reduce((sum,p)=>sum+p.speed,0)/recent.length:job.speed;
      const remaining=job.size!=null?Math.max(0,job.size-job.downloaded):null;
      if(remaining===0) eta.textContent='正在合并并校验文件…';
      else if(rate>0&&remaining!=null) {
        const seconds=Math.ceil(remaining/rate),hours=Math.floor(seconds/3600),minutes=Math.floor(seconds%3600/60);
        const duration=hours?`${hours} 小时 ${minutes} 分钟`:minutes?`${minutes} 分 ${seconds%60} 秒`:`${seconds} 秒`;
        eta.textContent=`预计剩余 ${duration} · 预计完成 ${new Date(Date.now()+seconds*1000).toLocaleString()}（随速度更新）`;
      } else eta.textContent='正在估算完成时间…';
    }
    if(job.engine==='aria2'&&['torrent','magnet'].includes(job.kind))task.append(text('p',`上传 ${bytes(job.upload_speed)}/s · 累计上传 ${bytes(job.uploaded)} · 分享率 ${job.share_ratio.toFixed(2)} · 上传峰值 ${bytes(job.peak_upload_speed)}/s · 上传均值 ${bytes(job.average_upload_speed)}/s · 连接 ${job.peers} / 做种 ${job.seeders} · 分片 ${job.completed_pieces}/${job.pieces}`,'bt-stats'));
    task.append(metrics,speedChart(job),eta);
    if(job.error) task.append(text('p',job.error,'task-error')); if(job.sha256) task.append(text('p','SHA-256 '+job.sha256,'checksum')); (destinations.get(job.group_id||('single:'+job.id))||$('tasks')).append(task);
  }
}


async function loadAccounts() {
  accounts=await api('/accounts');renderDrives();$('account-list').replaceChildren();
  for(const account of accounts) {
    const row=text('article','','account-row');const info=text('div');info.append(text('strong',account.name),text('p',account.connected?'已连接 · '+account.mount_path:'未登录 · 连接后可浏览和下载'));
    const actions=text('div','','task-actions');actions.append(button(account.connected?'重新登录':'连接账号',()=>openAccount(account.id)));
    if(account.connected) actions.append(button('退出账号',async()=>{if(!confirm(`退出${account.name}？已下载的本地文件会保留。`))return;try{await api('/accounts/'+account.id,{method:'DELETE'});await loadAccounts();await loadRoots();await browse('/',1);}catch(e){toast(e.message);}}));
    row.append(info,actions);$('account-list').append(row);
  }
}
$('refresh-accounts').onclick=()=>loadAccounts().catch(e=>toast(e.message));
const methodNames={sms:'手机验证码',qr:'扫码登录',oauth:'官方授权',cookie:'导入 Cookie',credential:'已有授权'};
async function openAccount(key,mode) {
  const account=accounts.find(a=>a.id===key);if(!account)return;
  await endLogin();const revision=++loginRevision;$('account-title').textContent='连接'+account.name;$('account-content').replaceChildren();$('account-message').textContent='正在准备登录…';$('account-expiry').textContent='';$('restart-login').hidden=true;
  if(!$('account-dialog').open)$('account-dialog').showModal();
  mode=mode||account.methods[0];$('login-methods').replaceChildren();
  for(const method of account.methods) $('login-methods').append(button(methodNames[method],()=>openAccount(key,method),method===mode?'primary':''));
  try {
    const session=await api('/accounts/'+key+'/login',{method:'POST',body:JSON.stringify({mode})});
    if(!$('account-dialog').open||revision!==loginRevision){api('/logins/'+session.id,{method:'DELETE'}).catch(()=>{});return;}loginSession=session;renderLogin(session);loginPoll=setInterval(checkLogin,2200);
    $('restart-login').onclick=()=>openAccount(key,mode);
  }catch(e){if(revision===loginRevision){$('account-message').textContent=e.message;$('restart-login').hidden=false;}}
}
function thunderSmsForm(s) {
  const form=text('form','','credential-form');form.id='thunder-sms-form';
  const label=text('label','中国大陆手机号');label.htmlFor='thunder-phone';
  const phone=document.createElement('input');phone.id='thunder-phone';phone.type='tel';phone.inputMode='tel';phone.autocomplete='tel-national';phone.placeholder='11 位手机号';phone.maxLength=11;phone.pattern='1[3-9][0-9]{9}';phone.required=true;
  const send=button('获取验证码',async()=>{if(!phone.checkValidity()||!phone.value){phone.reportValidity();return;}form.dataset.busy='true';send.disabled=true;try{await finishAction({action:'send',phone:phone.value});}catch(e){await checkLogin();$('account-message').textContent=e.message;}finally{delete form.dataset.busy;if(loginSession)updateThunderSms(loginSession);}},'primary');send.id='thunder-send';
  const codeLabel=text('label','短信验证码');codeLabel.htmlFor='thunder-code';
  const code=document.createElement('input');code.id='thunder-code';code.inputMode='numeric';code.autocomplete='one-time-code';code.placeholder='4 至 8 位验证码';code.maxLength=8;code.pattern='[0-9]{4,8}';code.required=true;
  const submit=button('登录迅雷云盘',null,'primary');submit.id='thunder-submit';submit.type='submit';
  form.append(label,phone,send,codeLabel,code,submit,button('更换手机号',()=>openAccount('thunder','sms')),text('small','点击获取验证码才会发送短信。验证码仅用于本次登录；需要安全验证时请在迅雷官方页面完成。'));
  form.onsubmit=async event=>{event.preventDefault();if(!code.value||!code.checkValidity()){code.reportValidity();return;}form.dataset.busy='true';submit.disabled=true;try{await finishAction({action:'verify',code:code.value});}catch(e){await checkLogin();$('account-message').textContent=e.message;}finally{delete form.dataset.busy;if(loginSession)updateThunderSms(loginSession);}};
  return form;
}
function updateThunderSms(s) {
  if(!$('thunder-sms-form'))return;
  const remaining=Math.max(0,Math.ceil((s.next_send_at||0)/1000-Date.now()/1000));
  const pending=$('thunder-sms-form').dataset.busy==='true';
  $('thunder-send').disabled=pending||remaining>0;$('thunder-send').textContent=remaining?`${remaining} 秒后可重新获取`:s.verification_sent?'重新获取验证码':'获取验证码';
  $('thunder-phone').disabled=pending||!!s.verification_sent;$('thunder-code').disabled=$('thunder-submit').disabled=pending||!s.verification_sent;
  let box=$('thunder-verification');
  if(s.verify_url){
    if(!box){box=text('div');box.id='thunder-verification';$('account-content').append(box);}
    if(box.dataset.url!==s.verify_url){box.dataset.url=s.verify_url;box.replaceChildren();const link=text('a','打开迅雷官方安全验证 ↗');link.href=s.verify_url;link.target='_blank';link.rel='noopener noreferrer';const frame=document.createElement('iframe');frame.id='thunder-captcha-frame';frame.title='迅雷官方安全验证';frame.src=s.verify_url;frame.setAttribute('sandbox','allow-scripts allow-forms allow-same-origin allow-popups');box.append(link,frame);}
  }else if(box)box.remove();
}
setInterval(()=>{if(loginSession?.provider==='thunder'&&loginSession.mode==='sms')updateThunderSms(loginSession);},1000);
function renderLogin(s) {
  const content=$('account-content');content.replaceChildren();$('account-message').textContent=s.message;
  if(s.provider==='thunder'&&s.mode==='sms'){content.append(thunderSmsForm(s));updateThunderSms(s);
  }else if(s.form_url) {
    const frame=document.createElement('iframe');frame.id='official-login-form';frame.title='夸克官方手机验证码登录';frame.src=s.form_url;frame.referrerPolicy='no-referrer';frame.setAttribute('sandbox','allow-scripts allow-forms allow-same-origin');content.append(frame);
    content.append(text('small','这是夸克官方表单，手机号、短信和安全验证由你的浏览器直接提交。若表单无法加载，可切换扫码。'));
  }else if(s.qr_svg||s.qr_image) {
    const qr=document.createElement('img');qr.className='login-qr';qr.alt='使用'+accounts.find(a=>a.id===s.provider)?.name+' App 扫码登录';qr.src=s.qr_image||'data:image/svg+xml;charset=utf-8,'+encodeURIComponent(s.qr_svg);content.append(qr);$('restart-login').hidden=false;
  }else if(s.setup_required) {
    const link=text('a','打开 OpenList 授权助手 ↗','authorize-link');link.href=s.helper_url;link.target='_blank';link.rel='noopener noreferrer';content.append(link,text('p','选择 '+accounts.find(a=>a.id===s.provider)?.name+'，使用 OpenList 提供的参数登录官方账号。取得刷新令牌后回到这里导入。'),text('small','此方式由 OpenList 授权助手处理授权及后续令牌刷新；自建应用可在“已有授权”中填写自己的客户端信息。'),button('导入刷新令牌',()=>openAccount(s.provider,'credential'),'primary'));
  }else if(s.authorize_url) {
    const link=text('a','打开'+(accounts.find(a=>a.id===s.provider)?.name||'网盘')+'官方授权页 ↗','authorize-link');link.href=s.authorize_url;link.target='_blank';link.rel='noopener noreferrer';content.append(link,text('p','在官方页面完成登录并允许授权，工作台会自动连接。'));
  }else if(s.mode==='credential'||s.mode==='cookie') {
    const form=document.createElement('form');form.className='credential-form';const label=text('label',s.mode==='cookie'||s.provider==='quark'||s.provider==='115'?'账号 Cookie':'刷新令牌');label.htmlFor='credential-secret';const secret=document.createElement('input');secret.type='password';secret.id='credential-secret';secret.required=true;secret.autocomplete='off';secret.placeholder='粘贴授权信息';form.append(label,secret);
    if(s.provider==='baidu'&&s.mode==='credential') {const link=text('a','通过 OpenList 授权助手获取 ↗');link.href='https://api.oplist.org/';link.target='_blank';link.rel='noopener noreferrer';form.append(link,text('small','默认使用 OpenList 授权助手对应的刷新令牌；自建应用请由部署者配置官方授权。'));}
    if(['onedrive','google'].includes(s.provider)) {
      const helper=text('a','打开 OpenList 授权助手获取令牌 ↗');helper.href='https://api.oplist.org/';helper.target='_blank';helper.rel='noopener noreferrer';
      const source=document.createElement('select');source.name='source';source.setAttribute('aria-label','授权来源');
      for(const [value,label]of[['openlist','OpenList 默认应用'],['custom','我的自建应用']]){const option=text('option',label);option.value=value;source.append(option);}
      form.append(helper,text('small','默认应用的授权和刷新经过 OpenList 服务。自建应用的令牌直接向微软 / Google 刷新，请填写获取此令牌时使用的客户端信息。'),source);
      const fields=text('div','','oauth-fields');
      for(const [key,label]of[['client_id','客户端 ID'],['client_secret','客户端密钥'],...(s.provider==='onedrive'?[['redirect_uri','获取令牌时的回调地址']]:[])]){
        const labelEl=text('label',label);const input=document.createElement('input');input.name=key;input.type=key==='client_secret'?'password':'text';input.autocomplete='off';input.id='oauth-'+key;labelEl.htmlFor=input.id;fields.append(labelEl,input);
      }
      const update=()=>{fields.hidden=source.value!=='custom';for(const field of fields.querySelectorAll('input'))field.disabled=fields.hidden,field.required=!fields.hidden;};source.onchange=update;update();form.append(fields);
    }
    if(s.provider==='thunder') {form.append(text('p','推荐使用手机验证码或扫码登录；也可以填写已有的迅雷 App 刷新令牌。'));for(const [key,label]of[['client_id','客户端 ID（可选）'],['client_secret','客户端密钥（可选）'],['device_id','设备 ID（可选）']]){const input=document.createElement('input');input.name=key;input.type='password';input.placeholder=label;input.setAttribute('aria-label',label);input.autocomplete='off';form.append(input);}}
    const submit=button('连接网盘',null,'primary');submit.type='submit';form.append(submit);form.onsubmit=async e=>{e.preventDefault();submit.disabled=true;const body={secret:secret.value};for(const input of form.querySelectorAll('[name]:not(:disabled)'))body[input.name]=input.value;try{await finishAction(body);secret.value='';}catch(err){$('account-message').textContent=err.message;}finally{submit.disabled=false;}};content.append(form);
  }
}
window.addEventListener('message',async event=>{
  const frame=$('official-login-form');
  if(!loginSession||loginSession.mode!=='sms'||event.origin!=='https://uop.quark.cn'||!frame||event.source!==frame.contentWindow||typeof event.data!=='string'||!/^[a-zA-Z0-9]{32}$/.test(event.data))return;
  try{await finishAction({ticket:event.data});}catch(e){$('account-message').textContent=e.message;}
});
async function finishAction(body) {if(!loginSession)return;const s=await api('/logins/'+loginSession.id+'/action',{method:'POST',body:JSON.stringify(body)});await applyLoginStatus(s);}
async function checkLogin() {if(!loginSession||loginChecking)return;loginChecking=true;const id=loginSession.id;try{const s=await api('/logins/'+id);await applyLoginStatus(s);}catch(e){if(loginSession?.id===id)$('account-message').textContent=e.message;}finally{loginChecking=false;}}
async function applyLoginStatus(s) {
  if(!loginSession||s.id!==loginSession.id)return;
  loginSession=s;if(s.provider==='thunder'&&s.mode==='sms')updateThunderSms(s);
  $('account-message').textContent=s.message;const remaining=Math.max(0,Math.ceil(s.expires_at/1000-Date.now()/1000));$('account-expiry').textContent=remaining?'会话剩余 '+Math.floor(remaining/60)+' 分 '+remaining%60+' 秒':'';
  if(s.status==='connected'){await endLogin();$('account-dialog').close();toast('已连接网盘，待下载分享将自动继续');await loadAccounts();await loadRoots();if(s.mount_path)await browse(s.mount_path,1);await loadShares();}
  if(['expired','cancelled','failed'].includes(s.status)){clearInterval(loginPoll);$('restart-login').hidden=false;$('account-content').replaceChildren(text('p',s.status==='expired'?'登录已过期，请重新开始。':s.status==='failed'?'授权验证失败，请重新开始。':'已取消登录。'));}
}
async function endLogin(){++loginRevision;clearInterval(loginPoll);$('account-content').replaceChildren();const s=loginSession;loginSession=null;if(s)api('/logins/'+s.id,{method:'DELETE'}).catch(()=>{});}
$('close-account').onclick=()=>{$('account-dialog').close();endLogin();};$('account-dialog').addEventListener('cancel',()=>endLogin());
$('share-form').onsubmit=async event=>{
  event.preventDefault();$('share-submit').disabled=true;
  try{const s=await api('/shares',{method:'POST',body:JSON.stringify({url:$('share-url').value,passcode:$('share-code').value.trim()})});$('share-url').value='';$('share-code').value='';await loadShares();const account=accounts.find(a=>a.id===s.provider);if(account&&!account.connected)await openAccount(account.id);else await openShare(s.id);}catch(e){toast(e.message);}finally{$('share-submit').disabled=false;}
};
async function loadShares(){
  const shares=await api('/shares');$('share-requests').replaceChildren();
  for(const s of shares.slice(0,12)){
    const row=text('article','','share-request');const info=text('div');info.append(text('strong',accounts.find(a=>a.id===s.provider)?.name||s.provider),text('small',s.url),text('p',s.message));const actions=text('div','','task-actions');
    if(s.status==='waiting_login')actions.append(button('登录并继续',()=>openAccount(s.provider)));
    if(s.status==='failed')actions.append(button('重试',async()=>{try{await api('/shares/'+s.id+'/retry',{method:'POST'});await loadShares();}catch(e){toast(e.message);}}));
    if(['ready','queued','awaiting_transfer','insufficient_capacity'].includes(s.status))actions.append(button(s.has_contents?'浏览与选择':'查看内容',()=>openShare(s.id),'primary'));
    if(s.jobs.length)actions.append(button('查看下载',()=>setView('tasks')));
    actions.append(button(s.status==='importing'?'取消':'移除',async()=>{if(s.status==='importing'&&!confirm('取消后续下载？网盘已保存的副本会保留。'))return;try{await api('/shares/'+s.id,{method:'DELETE'});await loadShares();}catch(e){toast(e.message);}}));row.append(info,actions);$('share-requests').append(row);
  }
  if(typeof refreshShareDialog==='function')refreshShareDialog(shares);
}
async function cloudOperation(body){try{const r=await api('/files/operation',{method:'POST',body:JSON.stringify(body)});toast(r.submitted?'移动任务已提交，完成后刷新目录':'操作成功');await browse(currentPath,page,true);}catch(e){toast(e.message);}}
$('mkdir').onclick=()=>{const name=prompt('新文件夹名称');if(name)cloudOperation({action:'mkdir',path:currentPath,name});};
$('rename-cloud').onclick=()=>{const old=[...selected][0];if(!old)return;const name=prompt('新的名称',old);if(name&&name!==old)cloudOperation({action:'rename',path:join(old),name});};
$('move-cloud').onclick=()=>{const destination=prompt('目标文件夹路径（同一网盘），例如 /quark/资料');if(destination)cloudOperation({action:'move',path:currentPath,names:[...selected],destination});};
$('delete-cloud').onclick=()=>{if(selected.size&&confirm(`从网盘删除选中的 ${selected.size} 项内容？文件夹内的内容也将删除。`))cloudOperation({action:'delete',path:currentPath,names:[...selected]});};
async function loadLocal(){
  const files=await api('/local');$('local-files').replaceChildren();
  if(typeof appendTransferFiles==='function')await appendTransferFiles();
  if(!files.length&&!$('local-files').children.length){$('local-files').append(text('div','还没有已完成的本地文件。','empty'));return;}
  for(const file of files){const row=text('article','','account-row');const info=text('div');info.append(text('strong',file.name),text('p',bytes(file.size)+' · '+(file.modified?new Date(file.modified*1000).toLocaleString('zh-CN'):'时间未知')));if(file.sha256)info.append(text('small','SHA-256 '+file.sha256,'checksum'));if(file.passwords?.length)info.append(passwordNotes(file.passwords));const actions=text('div','','task-actions');const link=text('a','保存到设备');link.href='/api/local/'+file.id+'/file';link.download=file.name;actions.append(link,button('重命名',async()=>{const name=prompt('新的文件名',file.name);if(!name||name===file.name)return;try{await api('/local/'+file.id+'/rename',{method:'POST',body:JSON.stringify({name})});await loadLocal();await loadJobs();}catch(e){toast(e.message);}}),button('删除',async()=>{if(!confirm('删除服务器本地文件“'+file.name+'”？网盘原文件会保留。'))return;try{await api('/local/'+file.id,{method:'DELETE'});await loadLocal();await loadJobs();}catch(e){toast(e.message);}}));row.append(info,actions);$('local-files').append(row);}
}
$('refresh-local').onclick=()=>loadLocal().catch(e=>toast(e.message));

initialize();

$('upload-cloud').onclick=()=>{if(currentPath!=='/')$('upload-input').click();};
$('upload-input').onchange=async()=>{
  const file=$('upload-input').files[0];if(!file)return;
  if(file.size>64*1024*1024){toast('网页单次上传最多 64 MiB，请使用网盘官方客户端上传更大的文件');$('upload-input').value='';return;}
  const destination=currentPath;const control=$('upload-cloud');control.disabled=true;control.textContent='正在上传…';
  try {
    const res=await fetch('/api/files/upload?path='+encodeURIComponent(destination.replace(/\/$/,'')+'/'+file.name),{method:'PUT',headers:{'Content-Type':'application/octet-stream'},body:file});
    if(!res.ok){const e=await res.json().catch(()=>({error:'上传失败，请稍后重试'}));throw new Error(e.error||'上传失败');}
    toast('文件已上传');if(currentPath===destination)await browse(currentPath,1,true);
  }catch(e){toast(e.message);}finally{control.textContent='上传文件';control.disabled=currentPath==='/';$('upload-input').value='';}
};

window.addEventListener('message',async event=>{
  const frame=$('thunder-captcha-frame');
  if(loginSession?.provider!=='thunder'||loginSession.mode!=='sms'||!frame||event.source!==frame.contentWindow||!['https://i.xunlei.com','https://captcha.xunlei.com'].includes(event.origin))return;
  const value=event.data;if(!value||typeof value!=='object')return;
  const token=value.captcha_token||value.captchaToken;if(typeof token!=='string'||token.length<16||token.length>4096)return;
  try{await finishAction({action:'challenge',captcha_token:token});}catch(e){$('account-message').textContent=e.message;}
});

async function loadSmb(){
  const rows=await api('/connections');$('smb-connections').replaceChildren();
  for(const row of rows){const card=text('div','','smb-card');card.append(button('▰ '+row.name,()=>{setView('browse');browse(row.mount_path,1);}),text('small',row.status==='connected'?row.address+' / '+row.share:'连接不可用'),button('断开',async()=>{if(!confirm('断开 SMB 连接？远端文件不会删除。'))return;try{await api('/connections/'+row.id,{method:'DELETE'});await loadSmb();await loadRoots();if(currentPath===row.mount_path||currentPath.startsWith(row.mount_path+'/'))await browse('/',1);}catch(e){toast(e.message);}}));$('smb-connections').append(card);}
  if(!rows.length)$('smb-connections').append(text('small','连接 NAS 或局域网服务器，直接浏览和下载共享文件。'));
}
$('add-smb').onclick=()=>{$('smb-error').textContent='';$('smb-dialog').showModal();};$('close-smb').onclick=()=>$('smb-dialog').close();
$('smb-form').onsubmit=async event=>{event.preventDefault();const submit=$('smb-form').querySelector('[type=submit]');submit.disabled=true;try{const body=Object.fromEntries(new FormData($('smb-form')));body.port=Number(body.port);const result=await api('/connections/smb',{method:'POST',body:JSON.stringify(body)});$('smb-password').value='';$('smb-dialog').close();await loadSmb();await loadRoots();await browse(result.mount_path,1,true);}catch(e){$('smb-error').textContent=e.message;}finally{submit.disabled=false;}};
