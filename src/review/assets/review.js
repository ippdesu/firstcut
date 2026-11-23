"use strict";
function escapeHtml(v){
  return String(v).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}

const sessionStorageKey = 'firstcut.review.session';
function apiFetch(path, options={}){
  const url = new URL(path, location.origin);
  if(url.origin !== location.origin) throw new Error('复核请求必须留在当前服务');
  const headers = new Headers(options.headers);
  let session;
  try { session = localStorage.getItem(sessionStorageKey); } catch (_) {}
  if(session) headers.set('Authorization', 'Bearer ' + session);
  return fetch(url, {...options, headers, credentials:'omit', redirect:'error', cache:'no-store'});
}

const photoResources = new WeakMap();
const thumbQueue = new Set();
let thumbnailRequests = 0;
function cancelPhoto(img){
  lazyPhotos.unobserve(img);
  thumbQueue.delete(img);
  const resource = photoResources.get(img);
  if(!resource) return;
  resource.abort.abort();
  if(resource.url) URL.revokeObjectURL(resource.url);
  photoResources.delete(img);
  img.removeAttribute('src');
}
function clearPhotos(container){
  container.querySelectorAll('img').forEach(cancelPhoto);
  container.replaceChildren();
}
async function loadPhoto(img, endpoint, path){
  const resource = {abort:new AbortController(),url:null};
  photoResources.set(img, resource);
  try {
    const response = await apiFetch(`${endpoint}?p=${encodeURIComponent(path)}`, {signal:resource.abort.signal});
    if(!response.ok) throw new Error(response.status);
    const blob = await response.blob();
    if(resource.abort.signal.aborted) return;
    resource.url = URL.createObjectURL(blob);
    img.src = resource.url;
  } catch (_) {
    if(!resource.abort.signal.aborted) img.dispatchEvent(new Event('error'));
  }
}
const lazyPhotos = new IntersectionObserver(entries=>{
  for(const {target, isIntersecting} of entries){
    if(!isIntersecting || !target.isConnected) continue;
    lazyPhotos.unobserve(target);
    thumbQueue.add(target);
  }
  pumpThumbnails();
}, {rootMargin:'280px'});
function pumpThumbnails(){
  while(thumbnailRequests < 6 && thumbQueue.size){
    const target = thumbQueue.values().next().value;
    thumbQueue.delete(target);
    if(!target.isConnected) continue;
    thumbnailRequests++;
    loadPhoto(target, '/thumb', target.dataset.photo).finally(()=>{
      thumbnailRequests--;
      pumpThumbnails();
    });
  }
}
function thumbnail(img, path){
  img.dataset.photo = path;
  lazyPhotos.observe(img);
}
// ---------- 侧栏布局 ----------
const sidebar = document.getElementById('filterSidebar');
const sidebarResize = document.getElementById('sidebarResize');
const sidebarToggle = document.getElementById('sidebarToggle');
const sidebarBackdrop = document.getElementById('sidebarBackdrop');
function setSidebarWidth(value, save=false){
  const width = Math.max(190, Math.min(480, Math.round(value)));
  document.documentElement.style.setProperty('--sidebar-width', `${width}px`);
  sidebarResize.setAttribute('aria-valuenow', String(width));
  if(save) try { localStorage.setItem('fc_sidebar_width', String(width)); } catch (_) {}
}
try {
  const savedWidth = Number(localStorage.getItem('fc_sidebar_width'));
  if(Number.isFinite(savedWidth) && savedWidth > 0) setSidebarWidth(savedWidth);
} catch (_) {}
let sidebarDrag = null;
sidebarResize.addEventListener('pointerdown', e=>{
  if(e.button !== 0) return;
  sidebarDrag = {id:e.pointerId, x:e.clientX, width:sidebar.getBoundingClientRect().width};
  sidebarResize.setPointerCapture(e.pointerId);
  document.body.classList.add('resizing-sidebar');
  e.preventDefault();
});
sidebarResize.addEventListener('pointermove', e=>{
  if(!sidebarDrag || e.pointerId !== sidebarDrag.id) return;
  const max = Math.max(190, Math.min(480, innerWidth - 320));
  setSidebarWidth(Math.min(max, sidebarDrag.width + e.clientX - sidebarDrag.x));
});
function endSidebarDrag(e){
  if(!sidebarDrag || e.pointerId !== sidebarDrag.id) return;
  sidebarDrag = null;
  document.body.classList.remove('resizing-sidebar');
  setSidebarWidth(sidebar.getBoundingClientRect().width, true);
}
sidebarResize.addEventListener('pointerup', endSidebarDrag);
sidebarResize.addEventListener('pointercancel', endSidebarDrag);
sidebarResize.addEventListener('lostpointercapture', endSidebarDrag);
sidebarResize.addEventListener('dblclick', ()=>setSidebarWidth(230, true));
sidebarResize.addEventListener('keydown', e=>{
  const step = e.key === 'ArrowRight' ? 20 : e.key === 'ArrowLeft' ? -20 : 0;
  if(!step && e.key !== 'Home') return;
  e.preventDefault();
  const max = Math.max(190, Math.min(480, innerWidth - 320));
  setSidebarWidth(e.key === 'Home' ? 230 : Math.min(max, sidebar.getBoundingClientRect().width + step), true);
});
function setSidebarOpen(open){
  document.body.classList.toggle('sidebar-open', open);
  sidebarBackdrop.hidden = !open;
  sidebarToggle.setAttribute('aria-expanded', String(open));
}
sidebarToggle.addEventListener('click', ()=>setSidebarOpen(true));
document.getElementById('sidebarClose').addEventListener('click', ()=>setSidebarOpen(false));
sidebarBackdrop.addEventListener('click', ()=>setSidebarOpen(false));
document.addEventListener('keydown', e=>{
  if(e.key === 'Escape' && document.body.classList.contains('sidebar-open')) setSidebarOpen(false);
});
matchMedia('(max-width:720px)').addEventListener('change', e=>{ if(!e.matches) setSidebarOpen(false); });
const sidebarSections = ['directoryHelp','starsSection','scoresSection','usageHelp'];
try {
  const saved = JSON.parse(localStorage.getItem('fc_sidebar_sections') || '{}');
  for(const id of sidebarSections) document.getElementById(id).open = saved[id] === true;
} catch (_) {}
for(const id of sidebarSections){
  document.getElementById(id).addEventListener('toggle', ()=>{
    const state = Object.fromEntries(sidebarSections.map(key=>[key,document.getElementById(key).open]));
    try { localStorage.setItem('fc_sidebar_sections', JSON.stringify(state)); } catch (_) {}
  });
}
// ---------- 数据 ----------
let SNAPSHOT = null;        // {root, photos}
let jpgs = [];              // 只展示非 RAW 行（分数与 JPG 相同）
const PAGE = 500;
let page = 0;
let uiSession = null;
let localDesktop = false;
let serverIncludeRaw = true;

async function loadAppControls(){
  try {
    const response = await apiFetch('/api/control', {cache:'no-store'});
    const data = await response.json();
    localDesktop = data.local_desktop === true;
    serverIncludeRaw = data.include_raw !== false;
    document.getElementById('exportPickFolder').hidden = !localDesktop;
    syncExportRawOption();
    if(data.enabled){
      uiSession = data.session;
      document.getElementById('appControls').hidden = false;
      document.getElementById('folderBlock').hidden = false;
      document.getElementById('includeRaw').checked = serverIncludeRaw;
    }
  } catch (_) {}
}

async function appControl(action, skipConfirm=false){
  const message = action === 'exit'
    ? '退出程序并关闭本地服务？关闭浏览器标签页不会退出程序。'
    : action === 'choose' ? '重新选择照片目录？'
    : action === 'restart' ? '重启服务并重新载入照片？' : '';
  if(!skipConfirm && !window.confirm(message)) return;
  const controls = document.querySelectorAll('#appControls button, #chooseFolder, #includeRaw');
  controls.forEach(button => button.disabled = true);
  try {
    const response = await apiFetch('/api/control', {
      method:'POST', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({action})
    });
    if(!response.ok){
      const reason = await response.text();
      toast(reason || `操作失败：${response.status}`);
      controls.forEach(button => button.disabled = false);
      return;
    }
    if(action === 'exit'){
      try { localStorage.removeItem(sessionStorageKey); } catch (_) {}
      document.getElementById('serviceStopped').hidden = false;
      return;
    }
    document.getElementById('info').textContent = action === 'choose'
      ? '正在等待选择照片目录…' : '正在重启服务…';
    for(let attempt=0;attempt<600;attempt++){
      await new Promise(resolve => setTimeout(resolve, 500));
      try {
        const next = await apiFetch('/api/control', {cache:'no-store'}).then(r=>r.json());
        if(next.enabled && next.session !== uiSession){ location.reload(); return; }
      } catch (_) {}
    }
    document.getElementById('info').textContent = '重启未完成，请重新运行 firstcut-ui.exe。';
  } catch (error){
    toast('操作失败：' + error);
    controls.forEach(button => button.disabled = false);
  }
}
document.getElementById('restartService').addEventListener('click', () => appControl('restart'));
document.getElementById('exitService').addEventListener('click', () => appControl('exit'));
document.getElementById('chooseFolder').addEventListener('click', () => appControl('choose', true));
document.getElementById('includeRaw').addEventListener('change', event => {
  appControl(event.target.checked ? 'rawon' : 'rawoff', true);
});
document.getElementById('continueUnscored').addEventListener('click', () => {
  document.getElementById('missingScores').hidden = true;
});
document.getElementById('chooseScoredFolder').addEventListener('click', () => {
  document.getElementById('missingScores').hidden = true;
  appControl('choose', true);
});

const DIMS = ["sharpness","exposure","noise","composition","aesthetic"];
const DIM_NAMES = {sharpness:"清晰",exposure:"曝光",noise:"噪点",composition:"构图",aesthetic:"美学"};

const f = {
  search: "", stars: new Set(), keep: "all", group: "all", aggregate: true, faces: false,
  min: {total:0, sharpness:0, exposure:0, noise:0, composition:0, aesthetic:0},
  sort: "total",
};

// ---------- 初始化 ----------
function ingest(){
  jpgs = SNAPSHOT.photos.filter(p => !p.is_raw);
  // 先刷新连拍组选项并校正失效的 f.group，再调用 apply() 渲染网格
  rebuildGroupOptions();
  document.getElementById("info").textContent =
    `${SNAPSHOT.root} — ${jpgs.length} 张（已评分 ${jpgs.filter(p=>p.scores).length}）`;
  apply();
}
async function boot(){
  let res;
  for(;;){
    try { res = await apiFetch('/api/photos', {cache:'no-store'}); }
    catch (_) {
      document.getElementById('info').textContent = '正在连接本地服务…';
      document.getElementById('loadPhase').textContent = '正在连接本地服务…';
      await new Promise(resolve => setTimeout(resolve, 500));
      continue;
    }
    if(res.status !== 503) break;
    try {
      const load = await apiFetch('/api/load', {cache:'no-store'}).then(r=>r.json());
      const count = load.total ? `${load.done} / ${load.total} 个文件` :
        load.done ? `已找到 ${load.done} 个文件` : '正在准备…';
      document.getElementById('info').textContent = `${load.phase} · ${count}`;
      document.getElementById('loadPhase').textContent = load.phase;
      document.getElementById('loadCount').textContent = count;
      document.getElementById('loadFill').style.width = load.total
        ? `${Math.min(100, Math.round(load.done / load.total * 100))}%` : '0%';
    } catch (_) {
      document.getElementById('info').textContent = '正在读取照片和评分…';
    }
    await new Promise(resolve => setTimeout(resolve, 500));
  }
  if(!res.ok){
    if(res.status === 401){
      try { localStorage.removeItem(sessionStorageKey); } catch (_) {}
      document.getElementById('info').textContent = '请登录';
      document.getElementById('loadPanel').hidden = true;
      document.getElementById('accessPanel').hidden = false;
      return false;
    }
    const error = await res.text();
    document.getElementById("info").textContent = "快照加载失败：" + error;
    document.getElementById('loadPhase').textContent = '加载失败：' + error;
    document.getElementById('loadCount').textContent = '可以点击左侧“选择照片目录”重试。';
    return false;
  }
  SNAPSHOT = await res.json();
  document.getElementById('loadPanel').hidden = true;
  ingest();
  buildFilters();
  document.querySelectorAll("#fSearch, #fStars input, #fKeep, #fGroup, #fAggregate, #fFaces, #fDims input, #fSort")
    .forEach(el => el.addEventListener("input", onFilter));
  await loadAppControls();
  if(uiSession !== null && jpgs.length && !jpgs.some(photo => photo.scores)){
    document.getElementById('missingScores').hidden = false;
  }
  return true;
}
function loadData(){
  apiFetch("/api/photos").then(r=>{
    if(!r.ok) throw new Error(r.status);
    return r.json();
  }).then(j=>{
    const currentPath = LB?.list?.[LB.idx]?.path;
    SNAPSHOT = j;
    ingest();
    if(currentPath){
      const list = visible();
      const idx = list.findIndex(p=>p.path===currentPath);
      if(idx < 0) closeLightbox();
      else { LB.list = list; LB.idx = idx; renderLB(); }
    }
  }).catch(()=>{});
}
function rebuildGroupOptions(){
  const groupCounts = new Map();
  for(const p of jpgs){
    const group = p.burst && p.burst.group;
    if(group) groupCounts.set(group, (groupCounts.get(group) || 0) + 1);
  }
  const groupBox = document.getElementById("fGroup");
  groupBox.querySelectorAll('option[value^="group:"]').forEach(opt => opt.remove());
  for(const [group, count] of [...groupCounts].sort((a,b)=>a[0]-b[0])){
    const option = document.createElement("option");
    option.value = `group:${group}`;
    option.textContent = `组 ${group}（${count} 张）`;
    groupBox.appendChild(option);
  }
  if(![...groupBox.options].some(opt => opt.value === f.group)){
    f.group = "all";
    page = 0;
  }
  groupBox.value = f.group;
}
function buildFilters(){
  const starBox = document.getElementById("fStars");
  for(let s=5; s>=1; s--){
    const l = document.createElement("label");
    l.innerHTML = `<input type="checkbox" data-star="${s}"> ${"★".repeat(s)}`;
    starBox.appendChild(l);
  }
  const dims = document.getElementById("fDims");
  for(const k of ["total", ...DIMS]){
    const name = k==="total" ? "总分" : DIM_NAMES[k];
    const wrap = document.createElement("label");
    wrap.innerHTML = `${name} <span class="dim" id="v_${k}">≥0</span>`;
    const r = document.createElement("input");
    r.type = "range"; r.min = 0; r.max = 100; r.value = 0; r.dataset.dim = k;
    wrap.appendChild(r);
    dims.appendChild(wrap);
  }
}
function onFilter(){
  f.search = document.getElementById("fSearch").value.trim().toLowerCase();
  f.stars = new Set([...document.querySelectorAll("#fStars input:checked")].map(i=>+i.dataset.star));
  f.keep = document.getElementById("fKeep").value;
  f.group = document.getElementById("fGroup").value || "all";
  f.aggregate = document.getElementById("fAggregate").checked;
  f.faces = document.getElementById("fFaces").checked;
  document.querySelectorAll("#fDims input").forEach(r=>{
    f.min[r.dataset.dim] = +r.value;
    document.getElementById("v_"+r.dataset.dim).textContent = "≥"+r.value;
  });
  f.sort = document.getElementById("fSort").value;
  updateFilterCounts();
  page = 0;
  localStorage.setItem("fc_filters", JSON.stringify({search:f.search,stars:[...f.stars],keep:f.keep,group:f.group,aggregate:f.aggregate,faces:f.faces,min:f.min,sort:f.sort}));
  apply();
}
function resetFilters(){
  document.getElementById("fSearch").value = "";
  document.querySelectorAll("#fStars input").forEach(i=>i.checked=false);
  document.getElementById("fKeep").value = "all";
  document.getElementById("fGroup").value = "all";
  document.getElementById("fAggregate").checked = true;
  document.getElementById("fFaces").checked = false;
  document.querySelectorAll("#fDims input").forEach(r=>{ r.value = 0; });
  document.getElementById("fSort").value = "total";
  onFilter();
}

// ---------- 过滤/排序/分页 ----------
function visible(){
  let list = jpgs.filter(p=>{
    if(f.search && !p.filename.toLowerCase().includes(f.search)) return false;
    if(f.stars.size && !(p.stars && f.stars.has(p.stars))) return false;
    const group = p.burst && p.burst.group;
    if(f.keep === "keep" && group && !p.burst.keep) return false;
    if(f.keep === "drop" && !(group && !p.burst.keep)) return false;
    if(f.group === "burst" && !group) return false;
    if(f.group === "single" && group) return false;
    if(f.group.startsWith("group:") && String(group || "") !== f.group.slice(6)) return false;
    if(f.faces && !(p.faces > 0)) return false;
    if(!p.scores) return !(f.min.total>0 || DIMS.some(d=>f.min[d]>0) || f.stars.size);
    if(p.scores.total < f.min.total) return false;
    for(const d of DIMS) if(p.scores[d] < f.min[d]) return false;
    return true;
  });
  const by = f.sort;
  const key = by.replace("-","");
  const descending = key === "total" ? !by.startsWith("-") : key === "time";
  list.sort((a,b)=>{
    let va, vb;
    if(key === "total"){ va = a.scores?a.scores.total:-1; vb = b.scores?b.scores.total:-1; }
    else if(key === "time"){ va = a.datetime || ""; vb = b.datetime || ""; }
    else { va = a.path; vb = b.path; }
    return va < vb ? (descending ? 1 : -1) : va > vb ? (descending ? -1 : 1) : 0;
  });
  return list;
}
function updateFilterCounts(){
  document.getElementById('starFilterCount').textContent = f.stars.size ? `已选 ${f.stars.size} 档` : '';
  const count = Object.values(f.min).filter(value=>Number(value)>0).length;
  document.getElementById('scoreFilterCount').textContent = count ? `已设 ${count} 项` : '';
}
function aggregatePhotos(list){
  if(!f.aggregate || f.group.startsWith("group:")) return list;
  const seen = new Set();
  return list.filter(p=>{
    const group = p.burst && p.burst.group;
    if(!group) return true;
    if(seen.has(group)) return false;
    seen.add(group);
    return true;
  });
}
function apply(){
  const matching = visible();
  const list = aggregatePhotos(matching);
  const pages = Math.max(1, Math.ceil(list.length / PAGE));
  if(page >= pages) page = pages - 1;
  const slice = list.slice(page*PAGE, page*PAGE + PAGE);
  const grid = document.getElementById("grid");
  clearPhotos(grid);
  document.getElementById("empty").style.display = list.length ? "none" : "block";
  for(const p of slice){
    const d = document.createElement("div");
    d.className = "card" + (p.scores ? "" : " noscore");
    d.dataset.path = p.path;
    d.innerHTML = `<img decoding="async">
      ${p.stars ? `<span class="b stars">${"★".repeat(p.stars)}</span>` : ""}
      ${p.scores ? `<span class="b total">${p.scores.total.toFixed(1)}</span>` : `<span class="b total">未评分</span>`}
      ${p.burst && p.burst.group ? `<span class="b burst ${p.burst.keep?"":"drop"}">组${p.burst.group} #${p.burst.rank}/${p.burst.size} ${p.burst.manual_keep!=null?(p.burst.keep?"手动保留":"手动舍弃"):(p.burst.keep?"✓":"✗")}</span>` : ""}
      <span class="b name">${escapeHtml(p.filename)}</span>`;
    d.addEventListener("click", ()=> openLightbox(p, matching));
    d.addEventListener("contextmenu", e=>{
      e.preventDefault();
      showPhotoContextMenu(p, e.clientX, e.clientY);
    });
    grid.appendChild(d);
    thumbnail(d.querySelector('img'), p.path);
  }
  const position = `${list.length ? page*PAGE+1 : 0}–${Math.min((page+1)*PAGE, list.length)} / ${list.length}`;
  document.getElementById("pgInfo").textContent = matching.length === list.length
    ? position : `${position} 项（符合条件 ${matching.length} 张）`;
  document.getElementById("pgPrev").disabled = page === 0;
  document.getElementById("pgNext").disabled = page >= pages-1;
}
document.getElementById("pgPrev").addEventListener("click", ()=>{ page--; apply(); });
document.getElementById("pgNext").addEventListener("click", ()=>{ page++; apply(); });
document.getElementById("fReset").addEventListener("click", resetFilters);

function hidePhotoContextMenu(){
  const menu = document.getElementById("contextMenu");
  menu.style.display = "none";
  menu.replaceChildren();
}
function showPhotoContextMenu(photo, x, y){
  const menu = document.getElementById("contextMenu");
  menu.replaceChildren();
  const addAction = (label, action, disabled=false)=>{
    const button = document.createElement("button");
    button.type = "button";
    button.role = "menuitem";
    button.textContent = label;
    button.disabled = disabled;
    button.addEventListener("click", e=>{
      e.stopPropagation();
      hidePhotoContextMenu();
      action();
    });
    menu.appendChild(button);
  };
  const group = photo.burst && photo.burst.group;
  if(group){
    const count = jpgs.filter(p=>p.burst && p.burst.group === group).length;
    addAction(`只看第 ${group} 组（${count} 张）`, ()=>{
      document.getElementById("fGroup").value = `group:${group}`;
      page = 0;
      onFilter();
    });
    addAction("只看连拍照片", ()=>{
      document.getElementById("fGroup").value = "burst";
      page = 0;
      onFilter();
    });
    addAction(`手动设为保留${photo.burst.keep ? " ✓" : ""}`, ()=>setBurstKeep(photo, true));
    addAction(`手动设为舍弃${!photo.burst.keep ? " ✓" : ""}`, ()=>setBurstKeep(photo, false));
    if(photo.burst.manual_keep != null){
      addAction(`恢复自动建议（${photo.burst.suggested_keep ? "保留" : "舍弃"}）`, ()=>setBurstKeep(photo, null));
    }
  } else {
    addAction("只看非连拍照片", ()=>{
      document.getElementById("fGroup").value = "single";
      page = 0;
      onFilter();
    });
  }
  if(f.group !== "all") addAction("清除组筛选", ()=>{
    document.getElementById("fGroup").value = "all";
    page = 0;
    onFilter();
  });
  menu.style.display = "block";
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, innerWidth - rect.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, innerHeight - rect.height - 8))}px`;
}
async function setBurstKeep(photo, keep){
  try{
    const response = await apiFetch('/api/burst/keep', {
      method:'POST', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({p:photo.path, keep})
    });
    const text = await response.text();
    if(!response.ok) throw new Error(text || response.statusText);
    const result = JSON.parse(text);
    for(const item of jpgs){
      if(item.path === photo.path && item.burst){
        item.burst.keep = result.keep == null ? item.burst.suggested_keep : result.keep;
        item.burst.manual_keep = result.keep;
      }
    }
    if(LB) renderLBActive();
    apply();
    toast(keep == null ? '已恢复自动保留建议' : (keep ? '已手动标记为保留' : '已手动标记为舍弃'));
  }catch(error){
    toast('连拍标记保存失败：'+error.message);
  }
}
document.addEventListener("click", e=>{
  if(!e.target.closest("#contextMenu")) hidePhotoContextMenu();
});
document.addEventListener("keydown", e=>{
  if(e.key === "Escape") hidePhotoContextMenu();
});
window.addEventListener("scroll", hidePhotoContextMenu, true);
window.addEventListener("resize", hidePhotoContextMenu);

// ---------- 灯箱（1:1 缩放 / 并排同步对比） ----------
let LB = null; // {list, idx, compare, sel:Set, activePath, view:{s, ax, ay, fx, fy}}
const lbEl = document.getElementById("lb");
let histogramVisible = true;
let histogramSamplePercent = 10;
try { histogramVisible = localStorage.getItem('fc_histogram_visible') !== 'false'; } catch (_) {}
const histogramCache = new Map();
let histogramChannels = new Set([0,1,2,3]);
let histogramPosition = null;
try {
  const savedChannels = JSON.parse(localStorage.getItem('fc_histogram_channels') || 'null');
  if(Array.isArray(savedChannels)) histogramChannels = new Set(savedChannels.filter(c=>[0,1,2,3].includes(c)));
  const savedPosition = JSON.parse(localStorage.getItem('fc_histogram_position') || 'null');
  if(savedPosition && Number.isFinite(savedPosition.left) && Number.isFinite(savedPosition.top)) histogramPosition = savedPosition;
} catch (_) {}
async function loadHistogramSettings(){
  try {
    const response = await apiFetch('/api/ui-settings', {cache:'no-store'});
    if(!response.ok) return;
    const settings = await response.json();
    const value = Number(settings.histogram_sample_percent);
    if(Number.isInteger(value) && value>=1 && value<=25) histogramSamplePercent = value;
    if(LB?.histogramOpen) renderLBHistogram();
  } catch (_) {}
}

function apertureLabel(value){
  const text = String(value || '').trim();
  return text ? (/^f\s*\//i.test(text) ? text : `f/${text}`) : '';
}
function shutterLabel(value){
  const text = String(value || '').trim();
  return text ? (/\s*(s|sec|seconds)$/i.test(text) ? text : `${text}s`) : '';
}
function focalLabel(value){
  const text = String(value || '').trim();
  return text ? (/\s*mm$/i.test(text) ? text : `${text}mm`) : '';
}

function openLightbox(p, list){
  LB = { list, idx: list.indexOf(p), compare:false, sel:new Set([p.path]), activePath:p.path, view:{s:1, ax:.5, ay:.5, fx:.5, fy:.5}, detailsOpen:true, histogramOpen:histogramVisible };
  lbEl.style.display = "flex";
  renderLB();
}
function closeLightbox(){
  lbEl.style.display = "none";
  LB = null;
  clearPhotos(document.getElementById('panes'));
  clearPhotos(document.getElementById('strip'));
}
document.getElementById("lbClose").addEventListener("click", closeLightbox);

function lbNavSet(){
  const cur = LB.list[LB.idx];
  if(cur.burst && cur.burst.group){
    const g = cur.burst.group;
    return LB.list.filter(p => p.burst && p.burst.group === g);
  }
  return LB.list;
}
function groupMembers(){
  const cur = LB.list[LB.idx];
  if(cur.burst && cur.burst.group){
    const g = cur.burst.group;
    return jpgs.filter(p => p.burst && p.burst.group === g);
  }
  return [cur];
}
function shownPhotos(){
  if(!LB) return [];
  const cur = LB.list[LB.idx];
  if(!LB.compare) return [cur];
  const shown = [...LB.sel].map(path => jpgs.find(p=>p.path===path)).filter(Boolean);
  return shown.length ? shown : [cur];
}
function activePhoto(){
  if(!LB) return null;
  return shownPhotos().find(p=>p.path===LB.activePath) || shownPhotos()[0];
}
function selectPane(path){
  if(!LB || !LB.compare || !LB.sel.has(path) || LB.activePath === path) return;
  LB.activePath = path;
  renderLBActive();
}
function renderLBActive(){
  const cur = activePhoto();
  if(!cur) return;
  document.getElementById("lbName").textContent = cur.filename;
  const exif = [
    cur.iso ? (/^ISO\s*/i.test(cur.iso) ? cur.iso : `ISO ${cur.iso}`) : '',
    apertureLabel(cur.f_number), shutterLabel(cur.shutter), focalLabel(cur.focal)
  ].filter(Boolean);
  const metadata =
    `${cur.stars ? "★".repeat(cur.stars)+" " : ""}${cur.scores ? "总分 "+cur.scores.total.toFixed(1) : "未评分"}`
    + (cur.burst && cur.burst.group ? ` · 组${cur.burst.group} 第${cur.burst.rank}/${cur.burst.size}名（姿势簇${cur.burst.pose_cluster}）${cur.burst.manual_keep!=null?" 手动":""}${cur.burst.keep?" ✓保留":" ✗舍弃"}` : "")
    + (exif.length ? ` · ${exif.join(' · ')}` : '');
  document.getElementById("lbMeta").textContent = metadata;
  document.getElementById("lbMeta").title = metadata;
  document.querySelectorAll("#panes .pane").forEach(pane=>pane.classList.toggle("active", pane.dataset.path===cur.path));
  document.querySelectorAll("#strip .st").forEach(item=>item.classList.toggle("cur", item.dataset.path===cur.path));
  renderLbStars();
  renderLBDetails();
  renderLBHistogram();
}

function renderLB(){
  const cur = LB.list[LB.idx];
  document.getElementById("lbCompare").classList.toggle("on", LB.compare);

  // 窗格
  const panes = document.getElementById("panes");
  clearPhotos(panes);
  const shown = shownPhotos();
  if(!shown.some(p=>p.path===LB.activePath)) LB.activePath = shown[0].path;
  for(const p of shown){
    const pane = document.createElement("div");
    pane.className = "pane"; pane.dataset.path = p.path;
    pane.innerHTML = `<img>
      <span class="tag">${escapeHtml(p.filename)}${p.scores ? " · "+p.scores.total.toFixed(1) : ""}</span>
      <span class="hint">${LB.compare ? "点击查看评分 · " : ""}滚轮缩放 · 拖拽平移 · 双击复位</span>`;
    const img = pane.querySelector('img');
    for(const eventName of ['load','error']) img.addEventListener(eventName, ()=>{
      if(pane.isConnected && LB){
        layoutPanes();
        if(LB.activePath === p.path) renderLBHistogram();
      }
    });
    bindPane(pane);
    panes.appendChild(pane);
    loadPhoto(img, '/image', p.path);
  }
  requestAnimationFrame(()=> layoutPanes());

  // 底条（组内成员，供导航与对比勾选）
  const strip = document.getElementById("strip");
  clearPhotos(strip);
  for(const m of groupMembers()){
    const d = document.createElement("div");
    d.className = "st";
    d.dataset.path = m.path;
    const checked = LB.sel.has(m.path) ? "checked" : "";
    d.innerHTML = `<img decoding="async">
      ${LB.compare ? `<input type="checkbox" ${checked}>` : ""}
      ${m.burst ? `<span class="r">#${m.burst.rank}${m.burst.keep?"✓":"✗"}</span>` : ""}`;
    d.addEventListener("click", e=>{
      if(LB.compare && e.target.type === "checkbox"){
        if(e.target.checked){
          if(LB.sel.size >= 4){ e.target.checked = false; return; }
          LB.sel.add(m.path);
          LB.activePath = m.path;
        } else {
          if(LB.sel.size <= 1){ e.target.checked = false; return; }
          LB.sel.delete(m.path);
          if(LB.activePath === m.path) LB.activePath = [...LB.sel][0];
        }
        renderLB();
      } else if(LB.compare){
        if(LB.sel.has(m.path)) selectPane(m.path);
        else if(LB.sel.size < 4){ LB.sel.add(m.path); LB.activePath = m.path; renderLB(); }
        else toast('最多并排对比 4 张照片');
      } else {
        let idx = LB.list.findIndex(p=>p.path===m.path);
        if(idx < 0){ LB.list = groupMembers(); idx = LB.list.findIndex(p=>p.path===m.path); }
        LB.idx = idx;
        LB.activePath = m.path;
        renderLB();
      }
    });
    strip.appendChild(d);
    thumbnail(d.querySelector('img'), m.path);
  }
  renderLBActive();
}

// 最近邻缩图相当于抽取原始 JPEG 像素网格，约取全图 10%，避免插值抹平极值。不写缓存文件。
function histogramFromImage(img){
  const scale = Math.min(1, Math.sqrt(histogramSamplePercent/100));
  const width = Math.max(1, Math.ceil(img.naturalWidth * scale));
  const height = Math.max(1, Math.ceil(img.naturalHeight * scale));
  const sample = document.createElement('canvas');
  sample.width = width; sample.height = height;
  const ctx = sample.getContext('2d', {willReadFrequently:true});
  if(!ctx) throw new Error('浏览器未提供 Canvas 2D');
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(img, 0, 0, width, height);
  const pixels = ctx.getImageData(0, 0, width, height).data;
  const bins = Array.from({length:4}, ()=>new Uint32Array(256)); // 亮度、红、绿、蓝
  let dark = 0, bright = 0, count = 0;
  for(let i=0; i<pixels.length; i+=4){
    if(pixels[i+3] === 0) continue;
    const red = pixels[i], green = pixels[i+1], blue = pixels[i+2];
    const luminance = Math.round(.2126*red + .7152*green + .0722*blue);
    bins[0][luminance]++; bins[1][red]++; bins[2][green]++; bins[3][blue]++;
    if(luminance <= 5) dark++;
    if(luminance >= 250) bright++;
    count++;
  }
  return {bins, dark, bright, count, totalPixels:img.naturalWidth*img.naturalHeight};
}
function drawHistogram(histogram){
  const canvas = document.getElementById('lbHistogramCanvas');
  const width = Math.max(1, canvas.clientWidth);
  const height = Math.max(1, canvas.clientHeight);
  const pixelRatio = Math.min(2, window.devicePixelRatio || 1);
  canvas.width = Math.round(width * pixelRatio);
  canvas.height = Math.round(height * pixelRatio);
  const ctx = canvas.getContext('2d');
  if(!ctx) return;
  ctx.setTransform(pixelRatio,0,0,pixelRatio,0,0);
  ctx.fillStyle = '#111'; ctx.fillRect(0,0,width,height);
  const plot = {left:30, right:width-7, top:7, bottom:height-22};
  const plotHeight = Math.max(1, plot.bottom-plot.top);
  ctx.font = '9px system-ui, sans-serif'; ctx.fillStyle = '#999'; ctx.strokeStyle = '#333'; ctx.lineWidth = 1;
  ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
  for(const [fraction,label] of [[1,'100%'],[.5,'50%'],[0,'0%']]){
    const y = plot.bottom-fraction*plotHeight;
    ctx.beginPath(); ctx.moveTo(plot.left,y); ctx.lineTo(plot.right,y); ctx.stroke();
    ctx.fillText(label,plot.left-4,y);
  }
  const levels = [0,64,128,192,255];
  ctx.textBaseline = 'alphabetic';
  for(const [index,level] of levels.entries()){
    const x = plot.left+index*(plot.right-plot.left)/4;
    ctx.beginPath(); ctx.moveTo(x,plot.top); ctx.lineTo(x,plot.bottom); ctx.stroke();
    ctx.textAlign = index===0 ? 'left' : index===levels.length-1 ? 'right' : 'center';
    ctx.fillText(String(level),x,height-5);
  }
  let peak = 1;
  for(const channel of histogramChannels) for(const count of histogram.bins[channel]) peak = Math.max(peak, count);
  const colors = ['rgba(235,235,235,.95)','rgba(255,95,95,.8)','rgba(95,215,95,.8)','rgba(95,135,255,.8)'];
  for(const channel of [1,2,3,0]){
    if(!histogramChannels.has(channel)) continue;
    ctx.beginPath(); ctx.strokeStyle = colors[channel]; ctx.lineWidth = channel===0 ? 1.7 : 1.25;
    for(let i=0; i<256; i++){
      const x = plot.left + i * (plot.right-plot.left) / 255;
      const y = plot.bottom - histogram.bins[channel][i] / peak * plotHeight;
      if(i===0) ctx.moveTo(x,y); else ctx.lineTo(x,y);
    }
    ctx.stroke();
  }
}
function renderLBHistogram(){
  if(!LB) return;
  const panel = document.getElementById('lbHistogram');
  const toggle = document.getElementById('lbHistogramToggle');
  panel.hidden = !LB.histogramOpen;
  toggle.classList.toggle('on', LB.histogramOpen);
  toggle.setAttribute('aria-expanded', String(LB.histogramOpen));
  if(!LB.histogramOpen) return;
  positionHistogramPanel();
  renderHistogramChannels();
  const photo = activePhoto();
  if(!photo) return;
  document.getElementById('lbHistogramPhoto').textContent = photo.filename;
  const note = document.getElementById('lbHistogramNote');
  const pane = [...document.querySelectorAll('#panes .pane')].find(item=>item.dataset.path===photo.path);
  const img = pane?.querySelector('img');
  if(!img?.naturalWidth){
    const canvas = document.getElementById('lbHistogramCanvas');
    canvas.width = canvas.width;
    note.textContent = img?.complete ? '原图加载失败，无法生成直方图。' : '正在读取原图像素…';
    return;
  }
  const key = `${SNAPSHOT?.root || ''}|${photo.path}|${histogramSamplePercent}`;
  let histogram = histogramCache.get(key);
  if(!histogram){
    try { histogram = histogramFromImage(img); }
    catch(error){ note.textContent = `无法读取原图像素：${error.message}`; return; }
    histogramCache.set(key, histogram);
    if(histogramCache.size > 8) histogramCache.delete(histogramCache.keys().next().value);
  }
  drawHistogram(histogram);
  const percent = count => histogram.count ? `${(100*count/histogram.count).toFixed(1)}%` : '0%';
  const sampledPercent = histogram.totalPixels ? (100*histogram.count/histogram.totalPixels).toFixed(1) : `约${histogramSamplePercent}`;
  note.textContent = `JPEG 网格抽样 ${sampledPercent}% · ${histogram.count.toLocaleString()} 点（估计） · 暗部 ≤5：${percent(histogram.dark)} · 亮部 ≥250：${percent(histogram.bright)}`;
}
function positionHistogramPanel(){
  const panel = document.getElementById('lbHistogram');
  const toolbarHeight = document.querySelector('#lb .top').getBoundingClientRect().height;
  const paneTag = document.querySelector('#panes .pane.active .tag');
  const defaultTop = toolbarHeight + (paneTag?.offsetHeight || 20) + 22;
  const desired = histogramPosition || {left:12, top:defaultTop};
  const maxLeft = Math.max(8, window.innerWidth-panel.offsetWidth-8);
  const maxTop = Math.max(8, window.innerHeight-panel.offsetHeight-8);
  const position = {left:Math.max(8,Math.min(desired.left,maxLeft)), top:Math.max(8,Math.min(desired.top,maxTop))};
  panel.style.left = `${position.left}px`;
  panel.style.top = `${position.top}px`;
  histogramPosition = position;
}
function renderHistogramChannels(){
  document.querySelectorAll('#lbHistogram .hist-channel').forEach(button=>{
    const enabled = histogramChannels.has(Number(button.dataset.channel));
    button.classList.toggle('on', enabled);
    button.setAttribute('aria-pressed', String(enabled));
  });
}
function setHistogramOpen(open){
  if(!LB) return;
  LB.histogramOpen = open;
  histogramVisible = open;
  try { localStorage.setItem('fc_histogram_visible', String(open)); } catch (_) {}
  renderLBHistogram();
}
document.getElementById('lbHistogramToggle').addEventListener('click', ()=>setHistogramOpen(!LB?.histogramOpen));
document.getElementById('lbHistogramClose').addEventListener('click', ()=>setHistogramOpen(false));
document.querySelectorAll('#lbHistogram .hist-channel').forEach(button=>button.addEventListener('click', ()=>{
  const channel = Number(button.dataset.channel);
  if(histogramChannels.has(channel)) histogramChannels.delete(channel); else histogramChannels.add(channel);
  try { localStorage.setItem('fc_histogram_channels', JSON.stringify([...histogramChannels])); } catch (_) {}
  renderHistogramChannels();
  const photo = activePhoto();
  const histogram = photo && histogramCache.get(`${SNAPSHOT?.root || ''}|${photo.path}|${histogramSamplePercent}`);
  if(histogram) drawHistogram(histogram);
}));
const histogramDragHandle = document.querySelector('#lbHistogram .hist-head');
let histogramDrag = null;
histogramDragHandle.addEventListener('pointerdown', event=>{
  if(event.target.closest('button')) return;
  const rect = document.getElementById('lbHistogram').getBoundingClientRect();
  histogramDrag = {id:event.pointerId, dx:event.clientX-rect.left, dy:event.clientY-rect.top};
  histogramDragHandle.setPointerCapture(event.pointerId);
});
histogramDragHandle.addEventListener('pointermove', event=>{
  if(!histogramDrag || histogramDrag.id !== event.pointerId) return;
  histogramPosition = {left:event.clientX-histogramDrag.dx, top:event.clientY-histogramDrag.dy};
  positionHistogramPanel();
});
function finishHistogramDrag(event){
  if(!histogramDrag || histogramDrag.id !== event.pointerId) return;
  histogramDrag = null;
  try { localStorage.setItem('fc_histogram_position', JSON.stringify(histogramPosition)); } catch (_) {}
}
histogramDragHandle.addEventListener('pointerup', finishHistogramDrag);
histogramDragHandle.addEventListener('pointercancel', finishHistogramDrag);
window.addEventListener('resize', ()=>{ if(LB?.histogramOpen) renderLBHistogram(); });
const SCENE_OPTIONS = [
  ['portrait','人像 / 人物'],['stage','舞台 / 演出'],['highkey','高调 / 雪景'],
  ['sports','运动 / 飞鸟'],['lowlight','夜景 / 暗光'],['landscape','风光'],
  ['still_life','静物 / 展品'],['documentary','纪实 / 街拍'],['other','其他']
];
function sceneLabel(id){ return (SCENE_OPTIONS.find(([key])=>key===id)||[])[1] || id; }
function renderLBDetails(){
  const panel = document.getElementById('lbDetails');
  panel.style.display = LB.detailsOpen ? 'block' : 'none';
  document.getElementById('lbDetailsToggle').classList.toggle('on', LB.detailsOpen);
  document.getElementById('lbDetailsToggle').setAttribute('aria-expanded', String(LB.detailsOpen));
  if(!LB.detailsOpen) return;
  const photo = activePhoto();
  const picker = document.getElementById('lbDetailsPicker');
  picker.style.display = LB.compare ? 'flex' : 'none';
  if(LB.compare){
    document.getElementById('lbDetailsPhoto').innerHTML = shownPhotos().map((p,i)=>
      `<option value="${i}" ${p.path===photo.path?'selected':''}>${escapeHtml(p.filename)}</option>`).join('');
  }
  const weights = SNAPSHOT.weights || {};
  const body = document.getElementById('lbDetailsBody');
  const scores = photo.scores;
  const rows = scores ? DIMS.map(key=>{
    const score = Number(scores[key] || 0);
    const weight = Number(weights[key] || 0);
    const contribution = score * weight;
    return `<div class="score-row"><div class="score-line"><strong>${DIM_NAMES[key]}</strong><span>${score.toFixed(1)} × ${(weight*100).toFixed(0)}% = ${contribution.toFixed(2)} 分</span></div>
      <div class="score-track"><span style="width:${Math.max(0,Math.min(100,score))}%"></span></div></div>`;
  }).join('') : '<div class="detail-note">本张尚未评分，暂无分项与权重贡献。</div>';
  const hint = photo.scene_hint || {id:'unknown',label:'未识别',reason:'缺少场景数据。'};
  const feedback = photo.scene_feedback;
  const selected = feedback ? feedback.selected_scene : (hint.id==='unknown' ? '' : hint.id);
  body.innerHTML = `<h4>总分构成</h4>
    ${scores ? `<div><strong>${scores.total.toFixed(1)} 分</strong> · 五项子分乘以当前权重后相加，结果保留 1 位小数。</div>` : ''}
    ${rows}
    <p class="detail-note">这里显示当前快照实际使用的权重；编辑配置后需重新评分才能更新总分。</p>
    <h4>场景初判</h4>
    <div>${escapeHtml(hint.label)} <span class="detail-note">（规则线索）</span></div>
    <p class="detail-note">${escapeHtml(hint.reason)} 当前尚无通用场景分类模型，初判不会自动改变评分权重。</p>
    ${feedback ? `<p>已记录的实际场景：<strong>${escapeHtml(sceneLabel(feedback.selected_scene))}</strong></p>` : ''}
    <label for="lbSceneSelect">实际场景</label>
    <select id="lbSceneSelect"><option value="">请选择</option>${SCENE_OPTIONS.map(([id,label])=>`<option value="${id}" ${id===selected?'selected':''}>${label}</option>`).join('')}</select>
    <label for="lbSceneNote">补充说明（可选）</label>
    <textarea id="lbSceneNote" maxlength="1000" placeholder="例如：这是舞台演出，不是普通人像">${escapeHtml(feedback?.note || '')}</textarea>
    <button class="detail-save" id="lbSceneSave">保存场景记录</button>
    <p class="detail-note">记录会保存到照片目录的 .firstcut/scene-feedback.jsonl，供后续改进识别与评分。</p>`;
  document.getElementById('lbSceneSave').addEventListener('click', ()=>saveSceneFeedback(photo));
}
document.getElementById('lbDetailsPhoto').addEventListener('change', e=>{
  const photo = shownPhotos()[Number(e.target.value)];
  if(photo) selectPane(photo.path);
});
async function saveSceneFeedback(photo){
  const selected_scene = document.getElementById('lbSceneSelect').value;
  const note = document.getElementById('lbSceneNote').value.trim();
  if(!selected_scene){ toast('请先选择实际场景'); return; }
  try{
    const response = await apiFetch('/api/scene-feedback', {
      method:'POST', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({p:photo.path, selected_scene, note})
    });
    const text = await response.text();
    if(!response.ok) throw new Error(text || response.statusText);
    photo.scene_feedback = JSON.parse(text).record;
    renderLBDetails();
    toast('场景记录已保存');
  }catch(error){ toast('场景记录保存失败：'+error.message); }
}
document.getElementById('lbDetailsToggle').addEventListener('click', ()=>{
  if(!LB) return;
  LB.detailsOpen = !LB.detailsOpen;
  renderLBDetails();
});
document.getElementById('lbDetailsClose').addEventListener('click', ()=>{
  if(!LB) return;
  LB.detailsOpen = false;
  renderLBDetails();
});
const detailsPanel = document.getElementById('lbDetails');
const detailsHead = detailsPanel.querySelector('.head');
let detailsDrag = null;
function placeDetailsPanel(left, top){
  const maxLeft = Math.max(8, lbEl.clientWidth - detailsPanel.offsetWidth - 8);
  const maxTop = Math.max(48, lbEl.clientHeight - detailsPanel.offsetHeight - 8);
  detailsPanel.style.left = `${Math.max(8, Math.min(maxLeft, left))}px`;
  detailsPanel.style.top = `${Math.max(48, Math.min(maxTop, top))}px`;
}
detailsHead.addEventListener('pointerdown', e=>{
  if(e.button !== 0 || e.target.closest('button')) return;
  const rect = detailsPanel.getBoundingClientRect();
  const host = lbEl.getBoundingClientRect();
  detailsPanel.style.right = 'auto';
  detailsPanel.style.bottom = 'auto';
  detailsDrag = {id:e.pointerId, x:e.clientX, y:e.clientY, left:rect.left-host.left, top:rect.top-host.top};
  placeDetailsPanel(detailsDrag.left, detailsDrag.top);
  detailsHead.setPointerCapture(e.pointerId);
  e.preventDefault();
});
detailsHead.addEventListener('pointermove', e=>{
  if(!detailsDrag || e.pointerId !== detailsDrag.id) return;
  placeDetailsPanel(detailsDrag.left + e.clientX - detailsDrag.x,
    detailsDrag.top + e.clientY - detailsDrag.y);
});
for(const eventName of ['pointerup','pointercancel']){
  detailsHead.addEventListener(eventName, e=>{
    if(detailsDrag && e.pointerId === detailsDrag.id) detailsDrag = null;
  });
}
window.addEventListener('resize', ()=>{
  if(detailsPanel.style.left) placeDetailsPanel(parseFloat(detailsPanel.style.left), parseFloat(detailsPanel.style.top));
});
new ResizeObserver(()=>{
  if(detailsPanel.style.left) placeDetailsPanel(parseFloat(detailsPanel.style.left), parseFloat(detailsPanel.style.top));
}).observe(detailsPanel);
document.getElementById("lbCompare").addEventListener("click", function(){
  LB.compare = !LB.compare;
  const cur = LB.list[LB.idx];
  if(LB.compare){ LB.sel = new Set([cur.path]); const g = groupMembers().filter(m=>m.path!==cur.path); if(g[0]) LB.sel.add(g[0].path); }
  LB.activePath = cur.path;
  renderLB();
});

// 窗格缩放/平移：所有窗格将同一图像相对位置 (ax, ay) 放在同一窗格相对位置 (fx, fy)。
function bindPane(pane){
  pane.addEventListener("wheel", e=>{
    e.preventDefault();
    const img = pane.querySelector("img");
    if(!img.naturalWidth || !img.naturalHeight) return;
    if(pane.ox === undefined) layoutPanes();
    const rect = pane.getBoundingClientRect();
    const k = curK(pane);
    const fx = (e.clientX - rect.left) / rect.width;
    const fy = (e.clientY - rect.top) / rect.height;
    const ix = (e.clientX - rect.left - pane.ox) / (k * img.naturalWidth);
    const iy = (e.clientY - rect.top - pane.oy) / (k * img.naturalHeight);
    const factor = e.deltaY < 0 ? 1.25 : 1/1.25;
    LB.view = {s:Math.min(16, Math.max(1, LB.view.s * factor)), ax:ix, ay:iy, fx, fy};
    layoutPanes();
  }, {passive:false});
  let drag = null;
  pane.addEventListener("pointerdown", e=>{
    if(e.button !== 0) return;
    selectPane(pane.dataset.path);
    drag = {id:e.pointerId, x:e.clientX, y:e.clientY, ax:LB.view.ax, ay:LB.view.ay};
    pane.setPointerCapture(e.pointerId);
    pane.classList.add("dragging");
  });
  pane.addEventListener("pointermove", e=>{
    if(!drag || e.pointerId !== drag.id) return;
    const img = pane.querySelector("img");
    if(!LB || !img.naturalWidth || !img.naturalHeight) return;
    const k = curK(pane);
    LB.view.ax = drag.ax - (e.clientX - drag.x) / (k * img.naturalWidth);
    LB.view.ay = drag.ay - (e.clientY - drag.y) / (k * img.naturalHeight);
    layoutPanes();
  });
  function endPaneDrag(e){
    if(!drag || e.pointerId !== drag.id) return;
    drag = null;
    pane.classList.remove("dragging");
  }
  pane.addEventListener("pointerup", endPaneDrag);
  pane.addEventListener("pointercancel", endPaneDrag);
  pane.addEventListener("lostpointercapture", endPaneDrag);
  pane.addEventListener("dblclick", ()=>{
    LB.view = {s:1, ax:.5, ay:.5, fx:.5, fy:.5};
    layoutPanes();
  });
}
function curK(pane){
  const img = pane.querySelector("img");
  if(!img.naturalWidth) return 1;
  const fit = Math.min(pane.clientWidth / img.naturalWidth, pane.clientHeight / img.naturalHeight);
  return fit * LB.view.s;
}
function layoutPanes(){
  if(!LB) return;
  const zoom = document.getElementById('lbZoom');
  zoom.textContent = `缩放 ${Math.round(LB.view.s * 100)}%`;
  zoom.title = `相对于${LB.compare ? '各并排窗格' : '当前窗格'}适配尺寸的缩放倍率；100% 表示适配显示`;
  document.querySelectorAll(".pane").forEach(pane=>{
    const img = pane.querySelector("img");
    if(!img.naturalWidth) { img.onload = layoutPanes; return; }
    const k = curK(pane);
    pane.ox = LB.view.fx * pane.clientWidth - LB.view.ax * k * img.naturalWidth;
    pane.oy = LB.view.fy * pane.clientHeight - LB.view.ay * k * img.naturalHeight;
    img.style.transform = `translate(${pane.ox}px,${pane.oy}px) scale(${k})`;
  });
}

// 键盘
function isTypingTarget(target){ return target instanceof Element && !!target.closest('input,textarea,select,[contenteditable="true"]'); }
window.addEventListener("keydown", e=>{
  if(lbEl.style.display !== "flex") return;
  if(e.key === "Escape"){ LB && LB.compare ? (LB.compare = false, renderLB()) : closeLightbox(); }
  else if(isTypingTarget(e.target)) return;
  else if(e.key === "ArrowRight" || e.key === "ArrowLeft"){
    if(LB.compare){
      const shown = shownPhotos();
      let i = shown.findIndex(p=>p.path===LB.activePath);
      i = e.key === "ArrowRight" ? (i+1) % shown.length : (i-1+shown.length) % shown.length;
      selectPane(shown[i].path);
      return;
    }
    const set = lbNavSet();
    const cur = LB.list[LB.idx];
    let i = set.findIndex(p => p.path === cur.path);
    i = e.key === "ArrowRight" ? (i+1) % set.length : (i-1+set.length) % set.length;
    LB.idx = LB.list.indexOf(set[i]);
    LB.activePath = set[i].path;
    LB.view = {s:1, ax:.5, ay:.5, fx:.5, fy:.5};
    renderLB();
  }
});

// ---------- M-UI2：跑批 / 星级 / 配置 ----------
function toast(msg){
  const t = document.getElementById('toast');
  t.textContent = msg; t.style.display = 'block';
  clearTimeout(t._h); t._h = setTimeout(()=> t.style.display='none', 2600);
}
let jobPolling = null;
let jobIsRunning = false;
function formatBytes(bytes){
  if(bytes == null || !Number.isFinite(bytes)) return '未知';
  if(bytes >= 1024 ** 3) return `${(bytes / (1024 ** 3)).toFixed(2)} GB`;
  if(bytes >= 1024 ** 2) return `${(bytes / (1024 ** 2)).toFixed(1)} MB`;
  if(bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}
function pollJob(){
  apiFetch('/api/job').then(r=>r.json()).then(j=>{
    const isExport = j.kind === 'export';
    jobIsRunning = j.state === 'running';
    const bar = document.getElementById('runBarWrap');
    const barIn = document.getElementById('runBar');
    const st = document.getElementById('runState');
    const log = document.getElementById('runLog');
    const btn = document.getElementById('runBtn');
    const expBtn = document.getElementById('exportBtn');
    const expGroup = document.getElementById('exportJobGroup');
    const expBarWrap = document.getElementById('exportBarWrap');
    const expBar = document.getElementById('exportBar');
    const expState = document.getElementById('exportState');
    const expLog = document.getElementById('exportLog');
    const expCancel = document.getElementById('exportCancelJob');
    const expOpen = document.getElementById('exportOpenDir');

    if(j.state === 'running'){
      bar.style.display = 'block'; log.style.display = 'block';
      btn.disabled = true; expBtn.disabled = true;
      btn.textContent = isExport ? '导出中…' : '评分中…';
      const pct = j.total ? Math.round(j.done/j.total*100) : 0;
      barIn.style.width = pct + '%';
      st.style.display = 'block';
      st.style.color = 'var(--dim)';
      st.textContent = isExport
        ? `导出进度 ${j.done}/${j.total}（${pct}% · ${formatBytes(j.bytes_done||0)} / ${formatBytes(j.bytes_total||0)}）`
        : `进度 ${j.done}/${j.total}（${pct}%）`;
      log.textContent = (j.log||[]).join('\n');
      log.scrollTop = log.scrollHeight;
    } else {
      bar.style.display = 'none';
      btn.disabled = false; expBtn.disabled = false;
      btn.textContent = '▶ 重新评分';
      if(j.state === 'done' || j.state === 'failed'){
        st.style.display = 'block';
        st.textContent = j.state==='done' ? `✓ ${j.summary}` : `✗ ${j.error}`;
        st.style.color = j.state==='done' ? 'var(--acc)' : '#f44336';
      }
      log.style.display = (j.log||[]).length ? 'block' : 'none';
      if((j.log||[]).length) log.textContent = j.log.join('\n');
      // 仅评分任务完成时刷新快照数据
      if(!isExport && j.state === 'done' && jobPolling){ loadData(); }
    }

    if(isExport && j.state !== 'idle'){
      expGroup.hidden = false;
      if(j.state === 'running'){
        const pct = j.total ? Math.round(j.done/j.total*100) : 0;
        expBarWrap.style.display = 'block';
        expBar.style.width = pct + '%';
        expState.style.color = 'var(--dim)';
        const cur = j.current_file ? ` · 当前：${j.current_file}` : '';
        expState.textContent = `正在导出 ${j.done}/${j.total}（${pct}% · ${formatBytes(j.bytes_done||0)} / ${formatBytes(j.bytes_total||0)}）${cur}`;
        expCancel.hidden = !j.cancellable;
        expCancel.disabled = false;
        expOpen.hidden = true;
      } else {
        expBarWrap.style.display = 'none';
        expCancel.hidden = true;
        expState.textContent = j.state==='done' ? `✓ ${j.summary}` : `✗ ${j.error}`;
        expState.style.color = j.state==='done' ? 'var(--acc)' : '#f44336';
        expOpen.hidden = !(j.can_open_export && localDesktop);
        if(document.getElementById('exportModal').style.display === 'flex' && jobPolling){
          scheduleExportPreview(50);
        }
      }
      expLog.style.display = (j.log||[]).length ? 'block' : 'none';
      if((j.log||[]).length){
        expLog.textContent = j.log.join('\n');
        expLog.scrollTop = expLog.scrollHeight;
      }
    } else {
      expCancel.hidden = true;
      expOpen.hidden = true;
    }
    syncExportStartButton();

    // 运行中持续轮询；结束后停止
    if(j.state === 'running'){ schedulePoll(); }
    else { jobPolling = null; }
  }).catch(()=>{});
}
function schedulePoll(){ if(jobPolling) clearTimeout(jobPolling); jobPolling = setTimeout(pollJob, 600); }

// ---------- 导出筛选结果弹窗 ----------
const exportModal = document.getElementById('exportModal');
const exportSubdirEl = document.getElementById('exportSubdir');
const exportJpgEl = document.getElementById('exportJpg');
const exportRawEl = document.getElementById('exportRaw');
const exportXmpEl = document.getElementById('exportXmp');
const exportXmpExposureEl = document.getElementById('exportXmpExposure');
const exportLayoutEl = document.getElementById('exportLayout');
let exportExternalTargetId = null;
let exportExternalDisplay = '';
let exportPreviewData = null;
let exportPreviewTimer = null;
let exportPreviewSeq = 0;

function syncExportRawOption(){
  exportRawEl.disabled = !serverIncludeRaw;
  if(!serverIncludeRaw) exportRawEl.checked = false;
  document.getElementById('exportRawDisabledNote').hidden = serverIncludeRaw;
}
function syncExportXmpExposure(){
  const enabled = exportXmpEl.checked;
  exportXmpExposureEl.disabled = !enabled;
  if(!enabled) exportXmpExposureEl.checked = false;
}
function syncExportTargetUi(){
  const hasExternal = !!exportExternalTargetId;
  document.getElementById('exportSubdirRow').hidden = hasExternal;
  document.getElementById('exportExternalRow').hidden = !hasExternal;
  document.getElementById('exportExternalPath').textContent = exportExternalDisplay;
}
function syncExportStartButton(){
  const startBtn = document.getElementById('exportStart');
  const ok = !jobIsRunning && exportPreviewData && exportPreviewData.selected_photos > 0;
  startBtn.disabled = !ok;
}
function buildExportRequestPayload(){
  const xmp = exportXmpEl.checked;
  return {
    filter: {
      search: f.search,
      stars: [...f.stars],
      keep: f.keep,
      group: f.group,
      faces: f.faces,
      min: {...f.min},
    },
    subdir: exportExternalTargetId ? null : (exportSubdirEl.value.trim() || 'Selected'),
    external_target_id: exportExternalTargetId,
    layout: exportLayoutEl.value,
    include_jpg: exportJpgEl.checked,
    include_raw: serverIncludeRaw && exportRawEl.checked,
    include_xmp: xmp,
    xmp_exposure: xmp && exportXmpExposureEl.checked,
  };
}
function renderExportPreview(p){
  const box = document.getElementById('exportPreviewBox');
  const lines = [];
  lines.push(`<div><b>目标路径</b>：<code>${escapeHtml(p.target_display)}</code>${p.in_album ? ' <span class="dim">（相册内，写入 .firstcut-export 防重扫）</span>' : ' <span class="dim">（外部已授权目录）</span>'}</div>`);
  lines.push(`<div><b>命中照片</b>：${p.selected_photos} 张 · <b>待写入</b>：JPG ${p.jpg_to_copy} / RAW ${p.raw_to_copy} / XMP ${p.xmp_to_write}${p.xmp_generated ? `（新生成 ${p.xmp_generated}）` : ''}</div>`);
  const freeText = p.available_bytes != null ? ` · 目标卷可用 ${formatBytes(p.available_bytes)}` : '';
  lines.push(`<div><b>预计复制</b>：${formatBytes(p.total_copy_bytes)}${freeText} · <b>增量跳过</b>：${p.skipped_unchanged} 个 · <b>保留既有</b>：${p.protected_existing} 个</div>`);
  if(p.missing_raw_count > 0){
    const samples = (p.missing_raw_samples||[]).map(escapeHtml).join('、');
    lines.push(`<div class="warn">提示：${p.missing_raw_count} 张照片没有配对 RAW，已跳过其 RAW${samples ? `（例如：${samples}）` : ''}。</div>`);
  }
  if(p.missing_xmp_unscored > 0){
    lines.push(`<div class="warn">提示：${p.missing_xmp_unscored} 张未评分且无原 XMP 的照片不会生成新侧车。</div>`);
  }
  if(p.renamed_count > 0){
    const items = (p.renamed_samples||[]).map(c=>`<li>${escapeHtml(c.source)} → <code>${escapeHtml(c.exported_stem)}</code></li>`).join('');
    lines.push(`<div class="warn">检测到 ${p.renamed_count} 张跨目录同文件名照片，导出时将自动加目录前缀防覆盖：</div><ul>${items}</ul>`);
  }
  if(p.protected_existing > 0 && (p.protected_samples||[]).length){
    const items = p.protected_samples.map(s=>`<li><code>${escapeHtml(s)}</code></li>`).join('');
    lines.push(`<div class="warn">以下既有目标文件将保留不覆盖：</div><ul>${items}</ul>`);
  }
  for(const w of (p.warnings||[])){
    lines.push(`<div class="warn">⚠ ${escapeHtml(w)}</div>`);
  }
  box.innerHTML = lines.join('');
}
async function refreshExportPreview(){
  const seq = ++exportPreviewSeq;
  const box = document.getElementById('exportPreviewBox');
  const matched = visible();
  document.getElementById('exportFilterSummary').textContent =
    `基于当前左侧筛选命中的全部 ${matched.length} 张照片（不受“按连拍组聚合”折叠影响）。`;
  exportPreviewData = null;
  syncExportStartButton();
  box.innerHTML = '<span class="dim">正在计算导出预览…</span>';
  try {
    const response = await apiFetch('/api/export/preview', {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(buildExportRequestPayload()),
    });
    if(seq !== exportPreviewSeq) return;
    if(!response.ok){
      const reason = await response.text();
      box.innerHTML = `<div class="err">${escapeHtml(reason || `预览失败（${response.status}）`)}</div>`;
      return;
    }
    const data = await response.json();
    if(seq !== exportPreviewSeq) return;
    exportPreviewData = data;
    renderExportPreview(data);
    syncExportStartButton();
  } catch (err) {
    if(seq !== exportPreviewSeq) return;
    box.innerHTML = `<div class="err">预览失败：${escapeHtml(err.message || String(err))}</div>`;
  }
}
function scheduleExportPreview(delay=220){
  if(exportPreviewTimer) clearTimeout(exportPreviewTimer);
  exportPreviewTimer = setTimeout(refreshExportPreview, delay);
}
exportXmpEl.addEventListener('change', ()=>{ syncExportXmpExposure(); scheduleExportPreview(0); });
syncExportXmpExposure();
for(const el of [exportJpgEl, exportRawEl, exportXmpExposureEl, exportLayoutEl]){
  el.addEventListener('change', ()=> scheduleExportPreview(0));
}
exportSubdirEl.addEventListener('input', ()=> scheduleExportPreview(260));
document.getElementById('exportBtn').addEventListener('click', ()=>{
  syncExportRawOption();
  syncExportXmpExposure();
  syncExportTargetUi();
  exportModal.style.display = 'flex';
  refreshExportPreview();
});
document.getElementById('exportClose').addEventListener('click', ()=>{ exportModal.style.display = 'none'; });
document.getElementById('exportUseSubdir').addEventListener('click', ()=>{
  exportExternalTargetId = null;
  exportExternalDisplay = '';
  syncExportTargetUi();
  scheduleExportPreview(0);
});
document.getElementById('exportPickFolder').addEventListener('click', async ()=>{
  const btn = document.getElementById('exportPickFolder');
  btn.disabled = true;
  try {
    const response = await apiFetch('/api/export/pick-folder', {
      method:'POST', headers:{'Content-Type':'application/json'}, body:'{}',
    });
    if(!response.ok){
      toast(await response.text() || `无法打开文件夹选择器（${response.status}）`);
      return;
    }
    const data = await response.json();
    if(data.cancelled) return;
    if(data.in_album){
      exportExternalTargetId = null;
      exportExternalDisplay = '';
      exportSubdirEl.value = data.subdir || 'Selected';
    } else {
      exportExternalTargetId = data.target_id;
      exportExternalDisplay = data.display_path || '';
    }
    syncExportTargetUi();
    scheduleExportPreview(0);
  } catch (err) {
    toast('选择文件夹失败：' + err);
  } finally {
    btn.disabled = false;
  }
});
document.getElementById('exportStart').addEventListener('click', async ()=>{
  const btn = document.getElementById('exportStart');
  btn.disabled = true;
  try {
    const response = await apiFetch('/api/export/run', {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(buildExportRequestPayload()),
    });
    if(!response.ok){
      toast(await response.text() || `无法开始导出（${response.status}）`);
      syncExportStartButton();
      return;
    }
    document.getElementById('exportJobGroup').hidden = false;
    jobPolling = setTimeout(pollJob, 200);
  } catch (err) {
    toast('无法开始导出：' + err);
    syncExportStartButton();
  }
});
document.getElementById('exportCancelJob').addEventListener('click', async ()=>{
  const btn = document.getElementById('exportCancelJob');
  btn.disabled = true;
  try {
    const response = await apiFetch('/api/export/cancel', {
      method:'POST', headers:{'Content-Type':'application/json'}, body:'{}',
    });
    if(!response.ok) toast(await response.text() || '取消失败');
    else toast('已发送取消请求，当前文件写完后立即停止');
  } catch (err) {
    toast('取消失败：' + err);
  }
});
document.getElementById('exportOpenDir').addEventListener('click', async ()=>{
  try {
    const response = await apiFetch('/api/export/open', {
      method:'POST', headers:{'Content-Type':'application/json'}, body:'{}',
    });
    if(!response.ok) toast(await response.text() || '无法打开目录');
  } catch (err) {
    toast('无法打开目录：' + err);
  }
});
window.addEventListener('keydown', e=>{
  if(e.key === 'Escape' && exportModal.style.display === 'flex' && lbEl.style.display !== 'flex'){
    exportModal.style.display = 'none';
  }
});
const runXmpEl = document.getElementById('runXmp');
const runXmpExposureEl = document.getElementById('runXmpExposure');
function syncRunXmpExposure(){
  const enabled = runXmpEl.checked;
  runXmpExposureEl.disabled = !enabled;
  if(!enabled) runXmpExposureEl.checked = false;
}
runXmpEl.addEventListener('change', syncRunXmpExposure);
syncRunXmpExposure();
document.getElementById('runBtn').addEventListener('click', ()=>{
  const xmp = runXmpEl.checked;
  const xmpExposure = xmp && runXmpExposureEl.checked;
  apiFetch('/api/score/run', {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({xmp, xmp_exposure: xmpExposure, no_ai: document.getElementById('runNoAi').checked})
  }).then(async r=>{
    if(!r.ok){
      toast(await r.text() || `无法开始评分（${r.status}）`);
      return;
    }
    jobPolling = setTimeout(pollJob, 300);
  }).catch(err=> toast('无法开始评分：' + err));
});

// 星级：灯箱内按钮 + 快捷键
function renderLbStars(){
  const cur = activePhoto();
  const box = document.getElementById('lbStars');
  box.innerHTML = '';
  if(!cur || !cur.scores){ box.innerHTML = '<span class="dim" style="font-size:11px">未评分</span>'; return; }
  for(let s2=1; s2<=5; s2++){
    const b = document.createElement('button');
    b.textContent = s2 <= (cur.stars||0) ? '★' : '☆';
    b.title = `${s2} 星`;
    b.addEventListener('click', ()=> setStars(cur, s2));
    box.appendChild(b);
  }
}
function setStars(photo, n){
  apiFetch('/api/rate', {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({p: photo.path, stars: n})
  }).then(async r=>{
    const text = await r.text();
    let j = {};
    try { j = JSON.parse(text); } catch (_) {}
    if(r.ok && j.ok){
      toast(j.warnings?.length ? `已保存 ${n} 星；${j.warnings.length} 个侧车未同步` : `已保存 ${n} 星`);
      // 本地数据同步（同 stem 共享星级）；不重建窗格 DOM，避免重拉原图与重算直方图
      const stem = photo.path.replace(/\.[^.]+$/,'').toLowerCase();
      for(const p of (SNAPSHOT?.photos || jpgs)){
        if(p.path.replace(/\.[^.]+$/,'').toLowerCase() === stem){
          p.stars = n;
          p.rating_source = 'manual';
        }
      }
      if(LB) renderLBActive();
      apply();
    } else {
      toast(j.error || text || r.statusText || '写入失败');
    }
  }).catch(err=> toast('写入失败：' + err));
}
// 快捷键 1~5（灯箱打开时）
window.addEventListener('keydown', e=>{
  if(lbEl.style.display !== 'flex' || isTypingTarget(e.target)) return;
  if(e.key >= '1' && e.key <= '5'){
    const cur = activePhoto();
    if(cur && cur.scores) setStars(cur, +e.key);
  }
});

// 配置对话框
const FIELDS = [
  ['权重（和 ≈ 1.0）', [['w_sharpness','清晰度'],['w_exposure','曝光'],['w_noise','噪点'],['w_composition','构图'],['w_aesthetic','美学']]],
  ['星级', [['star_mode','分档模式（select:relative/absolute）'],['star_five_pct','5★ 百分位'],['star_four_pct','4★ 百分位'],['star_three_pct','3★ 百分位'],['star_two_pct','2★ 百分位'],['rating_5','5★ 阈值'],['rating_4','4★ 阈值'],['rating_3','3★ 阈值'],['rating_2','2★ 阈值']]],
  ['曝光 EV 容差带', [['exposure_target','目标亮度'],['exposure_ev_full_lo','暗侧满分'],['exposure_ev_full_hi','亮侧满分'],['exposure_ev_lo','暗侧归零'],['exposure_ev_hi','亮侧归零'],['exposure_subject_blend','主体混合权重'],['exposure_suggest_cap','建议EV上限']]],
  ['连拍去重', [['dedup_adaptive_keep','按姿态分组保留'],['dedup_keep_k','每姿势簇保留'],['dedup_pose_cluster_threshold','姿态阈值'],['dedup_burst_group_cap','单组上限']]],
];
const FIELD_GUIDE = {
  w_sharpness:{help:'清晰度维度对总分的占比；调高后更偏向合焦、细节清楚的照片。五项权重总和必须约为 1.0。',min:0,max:1,step:.01,recommended:'常用 0.15–0.40'},
  w_exposure:{help:'曝光维度对总分的占比；调高后更偏好主体曝光合适的照片。五项权重总和必须约为 1.0。',min:0,max:1,step:.01,recommended:'常用 0.15–0.35'},
  w_noise:{help:'暗部噪点维度的占比；调高后更严格地惩罚高 ISO 或暗部颗粒。五项权重总和必须约为 1.0。',min:0,max:1,step:.01,recommended:'常用 0.05–0.25'},
  w_composition:{help:'构图维度的占比；调高后 AI 构图评价对总分影响更大。五项权重总和必须约为 1.0。',min:0,max:1,step:.01,recommended:'常用 0.05–0.25'},
  w_aesthetic:{help:'美学维度的占比；调高后 AI 美学评价对总分影响更大。五项权重总和必须约为 1.0。',min:0,max:1,step:.01,recommended:'常用 0.05–0.25'},
  star_mode:{help:'relative 按本批次排名分星，适合每批照片质量差异较大的情况；absolute 按固定总分阈值分星，适合跨批次比较。'},
  star_five_pct:{help:'relative 模式下进入 5 星的前百分位比例；数值越小，5 星越少。',min:0,max:100,step:1,recommended:'常用 5–15%'},
  star_four_pct:{help:'relative 模式下 4 星及以上的累计百分位边界；必须不小于 5 星边界。',min:0,max:100,step:1,recommended:'常用 20–40%'},
  star_three_pct:{help:'relative 模式下 3 星及以上的累计百分位边界；必须不小于 4 星边界。',min:0,max:100,step:1,recommended:'常用 50–75%'},
  star_two_pct:{help:'relative 模式下 2 星及以上的累计百分位边界；其余照片为 1 星，且必须不小于 3 星边界。',min:0,max:100,step:1,recommended:'常用 80–95%'},
  rating_5:{help:'absolute 模式下获得 5 星的最低总分；需满足 2★ ≤ 3★ ≤ 4★ ≤ 5★。',min:0,max:100,step:1,recommended:'常用 70–90 分'},
  rating_4:{help:'absolute 模式下获得 4 星及以上的最低总分；需低于或等于 5 星阈值。',min:0,max:100,step:1,recommended:'常用 55–75 分'},
  rating_3:{help:'absolute 模式下获得 3 星及以上的最低总分；需低于或等于 4 星阈值。',min:0,max:100,step:1,recommended:'常用 40–60 分'},
  rating_2:{help:'absolute 模式下获得 2 星及以上的最低总分；低于此值为 1 星。',min:0,max:100,step:1,recommended:'常用 20–45 分'},
  exposure_target:{help:'理想主体亮度，单位为 8 位亮度值（0–255）；舞台暗调可降低，雪景或高调场景可提高。',min:32,max:224,step:1,recommended:'常用 96–160；有效范围 1–254'},
  exposure_ev_full_lo:{help:'曝光比目标偏暗多少 EV 以内仍给满分；增大可减少对轻微欠曝的惩罚，且必须小于暗侧归零值。',min:0,max:2,step:.05,recommended:'常用 0.5–1.5 EV'},
  exposure_ev_full_hi:{help:'曝光比目标偏亮多少 EV 以内仍给满分；增大可减少对轻微过曝的惩罚，且必须小于亮侧归零值。',min:0,max:2,step:.05,recommended:'常用 0.5–1.5 EV'},
  exposure_ev_lo:{help:'暗侧偏离达到该 EV 时曝光分降到 0；数值越大，对欠曝越宽容，必须大于暗侧满分容差。',min:1,max:8,step:.1,recommended:'常用 3–5 EV'},
  exposure_ev_hi:{help:'亮侧偏离达到该 EV 时曝光分降到 0；通常设得比暗侧更严格，必须大于亮侧满分容差。',min:1,max:6,step:.1,recommended:'常用 1.5–3 EV'},
  exposure_subject_blend:{help:'有主体脸时，主体亮度参与曝光判断的比例；0 只看全图，1 只看主体。',min:0,max:1,step:.05,recommended:'常用 0.3–0.7'},
  exposure_suggest_cap:{help:'记录到 XMP（firstcut:suggestedEV）与 CSV 的曝光修正建议最大绝对值（EV）；设为 0 可关闭建议。',min:0,max:4,step:.1,recommended:'常用 1–2 EV'},
  dedup_adaptive_keep:{help:'按人脸姿态把连拍拆成多个瞬间分别保留；关闭后退回按画面相似度分簇。'},
  dedup_keep_k:{help:'每个姿势簇保留分数最高的前 K 张；调大可保留更多表情或动作备选。',min:1,max:10,step:1,recommended:'常用 2–5 张；必须至少 1 张'},
  dedup_pose_cluster_threshold:{help:'姿态描述子的聚类距离阈值；调低会拆出更多姿态组，调高会合并相近姿势。',min:.05,max:.6,step:.01,recommended:'常用 0.15–0.40；默认 0.25'},
  dedup_burst_group_cap:{help:'单个连拍组最终保留照片总数上限，超过时按总分截断。',min:1,max:60,step:1,recommended:'常用 10–30 张'},
};
document.getElementById('cfgBtn').addEventListener('click', async ()=>{
  const [r, settingsResponse] = await Promise.all([apiFetch('/api/config'),apiFetch('/api/ui-settings')]);
  const j = await r.json(), settings = await settingsResponse.json();
  document.getElementById('cfgPath').textContent = '评分参数 → ' + j.path + (j.exists ? '' : '（文件尚不存在，保存时创建）');
  histogramSamplePercent = Number(settings.histogram_sample_percent) || 10;
  document.getElementById('histogramSampleNumber').value = histogramSamplePercent;
  document.getElementById('histogramSampleRange').value = histogramSamplePercent;
  document.getElementById('uiSettingsStatus').textContent = '';
  const v = j.values, body = document.getElementById('cfgBody');
  body.innerHTML = '';
  for(const [title, fields] of FIELDS){
    const g = document.createElement('div'); g.className = 'grp';
    g.innerHTML = `<b>${title}</b>`;
    for(const [k, label] of fields){
      const row = document.createElement('div'); row.className = 'cfg-field';
      const guide = FIELD_GUIDE[k];
      const line = document.createElement('div'); line.className = 'cfg-line';
      const name = document.createElement('span'); name.className = 'cfg-name'; name.textContent = label;
      line.appendChild(name);
      row.appendChild(line);
      if(k === 'star_mode'){
        const sel = document.createElement('select'); sel.dataset.k = k;
        sel.innerHTML = `<option value="relative" ${v[k]==='relative'?'selected':''}>relative 批次内相对</option><option value="absolute" ${v[k]==='absolute'?'selected':''}>absolute 总分阈值</option>`;
        line.appendChild(sel);
      } else if(k === 'dedup_adaptive_keep'){
        const checkWrap = document.createElement('label'); checkWrap.className = 'cfg-check';
        const check = document.createElement('input'); check.type = 'checkbox'; check.dataset.k = k; check.checked = !!v[k];
        checkWrap.appendChild(check); line.appendChild(checkWrap);
      } else {
        const inp = document.createElement('input');
        inp.type = 'number'; inp.step = String(guide.step); inp.dataset.k = k; inp.value = v[k]; inp.className = 'cfg-number';
        line.appendChild(inp);
        const rangeLine = document.createElement('div'); rangeLine.className = 'cfg-range';
        const minLabel = document.createElement('span'); minLabel.textContent = guide.min;
        const range = document.createElement('input'); range.type = 'range'; range.min = guide.min; range.max = guide.max; range.step = guide.step; range.value = Math.min(guide.max, Math.max(guide.min, Number(v[k])));
        range.setAttribute('aria-label', `${label}滑块`);
        const maxLabel = document.createElement('span'); maxLabel.textContent = guide.max;
        rangeLine.append(minLabel, range, maxLabel);
        const syncRange = ()=>{ range.value = Math.min(guide.max, Math.max(guide.min, Number(inp.value)||0)); };
        range.addEventListener('input', ()=>{ inp.value = range.value; });
        inp.addEventListener('input', syncRange);
        row.appendChild(rangeLine);
      }
      const help = document.createElement('div'); help.className = 'cfg-help';
      help.textContent = guide.help + (guide.recommended ? ` 建议：${guide.recommended}。滑块展示常用调节区间，仍可在输入框填写其他值。` : ' 具体有效范围与参数间约束会在保存时校验。');
      row.appendChild(help);
      g.appendChild(row);
    }
    body.appendChild(g);
  }
  document.getElementById('modal').style.display = 'flex';
});
document.getElementById('cfgCancel').addEventListener('click', ()=> document.getElementById('modal').style.display='none');
document.getElementById('histogramSampleRange').addEventListener('input', event=>{
  document.getElementById('histogramSampleNumber').value = event.target.value;
});
document.getElementById('histogramSampleNumber').addEventListener('input', event=>{
  const value = Math.max(1,Math.min(25,Number(event.target.value)||1));
  document.getElementById('histogramSampleRange').value = value;
});
document.getElementById('uiSettingsSave').addEventListener('click', async ()=>{
  const samplePercent = Number(document.getElementById('histogramSampleNumber').value);
  if(!Number.isInteger(samplePercent) || samplePercent<1 || samplePercent>25){
    toast('直方图抽样比例必须为 1% 到 25% 的整数'); return;
  }
  const button = document.getElementById('uiSettingsSave');
  button.disabled = true;
  try {
    const response = await apiFetch('/api/ui-settings', {
      method:'POST', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({histogram_sample_percent:samplePercent})
    });
    const body = await response.text();
    let result = {};
    try { result = JSON.parse(body); } catch (_) {}
    if(!response.ok) throw new Error(result.error || result.message || body || '保存失败');
    histogramSamplePercent = samplePercent;
    document.getElementById('histogramSampleRange').value = samplePercent;
    document.getElementById('uiSettingsStatus').textContent = '已保存在当前照片目录的 .firstcut/ui-preferences.json';
    if(LB?.histogramOpen) renderLBHistogram();
  } catch(error){ toast('界面设置保存失败：'+error.message); }
  finally { button.disabled = false; }
});
document.getElementById('cfgSave').addEventListener('click', ()=>{
  const values = {};
  document.querySelectorAll('#cfgBody [data-k]').forEach(el=>{
    values[el.dataset.k] = el.type === 'checkbox' ? el.checked : (el.type === 'number' ? parseFloat(el.value)||0 : el.value);
  });
  values.star_mode = document.querySelector('#cfgBody select[data-k]').value;
  apiFetch('/api/config', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(values)})
    .then(async r=>{
      const t = await r.text();
      toast(r.ok ? (JSON.parse(t).note || '已保存') : t);
      if(r.ok) document.getElementById('modal').style.display='none';
    })
    .catch(err=>toast('保存失败：'+err));
});

// 恢复上次的筛选条件
try{
  const saved = JSON.parse(localStorage.getItem("fc_filters") || "null");
  if(saved){
    f.search = saved.search || ""; f.keep = saved.keep || "all";
    f.group = saved.group || "all";
    f.aggregate = saved.aggregate !== false;
    f.faces = !!saved.faces; f.sort = saved.sort || "total";
    f.min = Object.assign(f.min, saved.min || {});
    f.stars = new Set(saved.stars || []);
  }
}catch(_){}
const accessPanel = document.getElementById('accessPanel');
const accessStatus = document.getElementById('accessStatus');
const accessKey = document.getElementById('accessKey');
async function signIn(key){
  const response = await fetch('/api/session', {
    method:'POST',
    headers:{'Content-Type':'application/json'},
    body:JSON.stringify({key}),
    cache:'no-store',
    credentials:'omit',
    redirect:'error',
  });
  if(!response.ok) throw new Error(await response.text() || '登录失败');
  const {session} = await response.json();
  if(typeof session !== 'string') throw new Error('登录响应无效');
  localStorage.setItem(sessionStorageKey, session);
}
document.getElementById('accessForm').addEventListener('submit', async event=>{
  event.preventDefault();
  const key = accessKey.value.trim();
  accessKey.value = '';
  accessStatus.textContent = '正在登录…';
  try {
    await signIn(key);
    location.reload();
  } catch (error) {
    accessStatus.textContent = error.message;
  }
});
async function startReview(){
  const launchKey = new URLSearchParams(location.hash.slice(1)).get('access');
  if(launchKey !== null){
    history.replaceState(null, '', '/'); // Don't retain the key in the address bar.
    try { await signIn(launchKey); }
    catch(error){
      document.getElementById('loadPanel').hidden = true;
      document.getElementById('info').textContent = '请登录';
      accessPanel.hidden = false;
      accessStatus.textContent = error.message;
      return false;
    }
  }
  return boot();
}
startReview().then(ready=>{
  if(!ready) return;
  loadHistogramSettings();
  pollJob();
  document.getElementById("fSearch").value = f.search;
  document.getElementById("fKeep").value = f.keep;
  document.getElementById("fGroup").value = f.group;
  document.getElementById("fAggregate").checked = f.aggregate;
  if(!document.getElementById("fGroup").value){
    f.group = "all";
    document.getElementById("fGroup").value = "all";
  }
  document.getElementById("fFaces").checked = f.faces;
  document.getElementById("fSort").value = f.sort;
  document.querySelectorAll("#fStars input").forEach(i=> i.checked = f.stars.has(+i.dataset.star));
  document.querySelectorAll("#fDims input").forEach(r=>{
    r.value = f.min[r.dataset.dim] || 0;
    document.getElementById("v_"+r.dataset.dim).textContent = "≥"+r.value;
  });
  updateFilterCounts();
  apply();
});
