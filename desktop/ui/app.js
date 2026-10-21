"use strict";
const $=id=>document.getElementById(id);
const escape=s=>String(s??"").replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let state=null,busy=false,lastAccounts="",lastEvents="",limits={},identities={};
const titles={overview:"Всё под рукой",accounts:"Аккаунты и квоты",activity:"События реле",settings:"Настройки"};
function problem(error){
  const raw=String(error);
  let message="Не удалось выполнить действие. Попробуй ещё раз; причина указана в подробностях.";
  if(/^[А-ЯЁ]/.test(raw))message=raw;
  else if(/OpenAI usage endpoint returned (401|403)/.test(raw))message="Не удалось подтвердить сессию. Войди в эту подписку заново.";
  else if(/OpenAI usage request failed/.test(raw))message="Не удалось получить квоты. Проверь подключение к интернету и попробуй ещё раз.";
  else if(/Python 3\.10\+|Windows Store aliases/.test(raw))message="Для входа нужен рабочий Python 3.10 или новее. Установи Python или укажи его путь в RELAY_PYTHON.";
  else if(/Port \d+ is already in use/.test(raw))message="Порт занят другой программой. Выбери свободный порт в настройках.";
  else if(/WinError 10048|Address already in use|address already in use/.test(raw))message="Порт для входа занят. Заверши попытку входа в другой программе и повтори.";
  else if(/not writable|Permission denied|Access is denied|Отказано в доступе|PermissionError/.test(raw))message="Не удалось записать данные реле. Перенеси его папку в доступное для записи место и повтори.";
  else if(/Login timed out/.test(raw))message="Время ожидания входа истекло. Начни вход заново.";
  else if(/login is already in progress/i.test(raw))message="Вход уже начат. Заверши его в браузере или нажми «Отменить».";
  return `<div>${escape(message)}</div>${message===raw?"":`<details class="error-details"><summary>Технические подробности</summary><pre>${escape(raw)}</pre></details>`}`;
}
function notice(message){$("notice").innerHTML=message?problem(message):"";$("notice").hidden=!message;}
async function invoke(op,slot=null,port=null){return window.__TAURI__.core.invoke("relay_command",{op,slot,port});}
function page(id){for(const node of document.querySelectorAll(".page"))node.hidden=node.id!==id;for(const node of document.querySelectorAll("nav button"))node.classList.toggle("selected",node.dataset.page===id);$("page-title").textContent=titles[id];}
function quotaWindow(window,label){
  if(!window||window.remaining_percent==null)return `<div class="quota-note">${escape(label)}: нет данных</div>`;
  const remaining=Math.max(0,Math.min(100,Number(window.remaining_percent)));
  let reset="";if(window.reset_at)reset=new Date(window.reset_at*1000).toLocaleString("ru-RU",{day:"numeric",month:"short",hour:"2-digit",minute:"2-digit"});
  return `<div class="quota"><div class="quota-line"><span>${escape(label)}</span><strong>Осталось ${Math.round(remaining)}%</strong></div><progress max="100" value="${remaining}"></progress>${reset?`<div class="quota-reset">Сброс ${escape(reset)}</div>`:""}</div>`;
}
function quotaPanel(slot){
  const result=limits[slot];
  if(!result)return `<div class="quota-note">Нажми «Проверить квоты», чтобы получить данные подписки.</div>`;
  if(result.loading)return `<div class="quota-note">Проверяем квоты…</div>`;
  if(result.error)return `<div class="quota-error">${problem(result.error)}</div>`;
  let html=quotaWindow(result.rate_limit?.primary_window,"Основное окно")+quotaWindow(result.rate_limit?.secondary_window,"Недельное окно");
  for(const extra of result.additional_rate_limits||[]){html+=`<div class="quota-note">${escape(extra.limit_name||extra.name||"Дополнительная квота")}</div>`+quotaWindow(extra.rate_limit?.primary_window,"Основное окно")+quotaWindow(extra.rate_limit?.secondary_window,"Недельное окно");}
  if(result.fetched_at)html+=`<div class="quota-note">Обновлено ${escape(new Date(result.fetched_at).toLocaleTimeString("ru-RU",{hour:"2-digit",minute:"2-digit"}))}</div>`;
  return html;
}
function accountCard(account,detailed){
  const slot=account.slot,secondary=slot==="secondary",name=secondary?"Вторая подписка":"Первая подписка";
  const button=(op,title,cls="secondary")=>`<button class="${cls}" data-op="${op}" data-slot="${slot}">${title}</button>`;
  return `<article class="account card ${secondary?"secondary-slot":""}"><div class="account-top"><span class="avatar">${secondary?"Ⅱ":"Ⅰ"}</span><div class="account-name">${name}<small>${slot}</small></div>${account.active?'<span class="active-label">● Активна</span>':""}</div>${account.configured?`<p class="email">${escape(account.email||"Аккаунт подключён")}</p><p class="account-caption">${escape(account.error||account.plan||"Подписка ChatGPT")}</p>`:'<div class="empty-icon">＋</div><p class="email">Аккаунт не подключён</p><p class="account-caption">Войди через браузер, чтобы использовать подписку.</p>'}<div class="account-actions">${!account.configured?button("login","Войти в ChatGPT","primary"):(account.active?"":button("switch","Использовать","primary"))}${account.configured&&detailed?button("limits","Проверить квоты")+button("login","Сменить вход")+button("logout","Выйти","danger"):""}</div>${detailed&&account.configured?`<div class="quota-container">${quotaPanel(slot)}</div>`:""}</article>`;
}
function renderAccounts(force=false){
  if(!state)return;
  const signature=JSON.stringify([state.accounts,limits]);if(signature===lastAccounts&&!force)return;lastAccounts=signature;
  $("overview-accounts").innerHTML=state.accounts.map(a=>accountCard(a,false)).join("");$("account-cards").innerHTML=state.accounts.map(a=>accountCard(a,true)).join("");
}
function render(next){
  const previousPending=state?.login?.pending;state=next;
  if(previousPending&&!state.login.pending&&!state.login.error&&state.login.slot)delete limits[state.login.slot];
  for(const account of state.accounts){const identity=JSON.stringify([account.email,account.plan,account.configured]);if(identities[account.slot]!==identity){delete limits[account.slot];identities[account.slot]=identity;}}
  $("sidebar-version").textContent=`Praxis Relay ${state.version}`;
  $("server-badge").textContent=state.running?"● API работает":"● API остановлен";$("server-badge").classList.toggle("online",state.running);
  $("server-toggle").textContent=state.running?"Остановить API":"Запустить API";
  $("base-url").textContent=state.base_url;$("api-key").textContent=state.api_key||"—";
  $("connection-state").textContent=state.running?(state.accounts.some(a=>a.configured)?"Готов принимать запросы":"Подключи аккаунт для запросов"):"Запусти API для подключения";
  $("auth-dir").textContent=state.auth_directory;$("app-version").textContent=state.version;$("protocol-version").textContent=state.protocol_version;
  if(document.activeElement!==$("port"))$("port").value=state.port;$("port").disabled=state.running;$("save-port").disabled=state.running;
  $("login-banner").hidden=!state.login.pending;$("login-link").hidden=!state.login_url;
  if(state.login_url)$("login-link").href=state.login_url;
  if(previousPending&&!state.login.pending&&state.login.error)notice(state.login.error);
  renderAccounts();
  const events=JSON.stringify(state.events);if(events!==lastEvents){lastEvents=events;$("events").innerHTML=state.events.map(e=>`<div class="event"><time>${escape(e.time)}</time><span>${escape(e.message)}</span></div>`).join("");}
}
async function action(op,slot=null,port=null){
  if(busy)return;busy=true;notice("");
  try{
    if(op==="logout"&&!window.confirm(`Выйти из ${slot==="secondary"?"второй":"первой"} подписки в этом реле?`))return;
    if(op==="limits"){limits[slot]={loading:true};renderAccounts();try{limits[slot]=await invoke(op,slot);}catch(error){limits[slot]={error:String(error)};}renderAccounts();}
    else render(await invoke(op,slot,port));
  }catch(error){notice(String(error));}finally{busy=false;}
}
document.addEventListener("click",async event=>{
  const button=event.target.closest("button");if(!button)return;
  if(button.dataset.page)page(button.dataset.page);
  if(button.dataset.go)page(button.dataset.go);
  if(button.dataset.op)await action(button.dataset.op,button.dataset.slot);
  if(button.dataset.copy){try{await navigator.clipboard.writeText($(button.dataset.copy).textContent);const old=button.textContent;button.textContent="Готово";setTimeout(()=>{button.textContent=old;},1200);}catch{notice("Не удалось скопировать автоматически. Выдели адрес и нажми Ctrl+C.");}}
});
$("server-toggle").addEventListener("click",()=>action(state?.running?"stop":"start"));
$("cancel-login").addEventListener("click",()=>action("cancel_login"));
$("save-port").addEventListener("click",()=>{const port=Number($("port").value);if(!Number.isInteger(port)||port<1||port>65535){notice("Порт должен быть целым числом от 1 до 65535.");return;}action("port",null,port);});
async function refresh(){try{if(!busy)render(await invoke("status"));}catch(error){notice(String(error));}}
refresh();setInterval(refresh,1500);
$("login-link").addEventListener("click",event=>{event.preventDefault();action("open_login");});
