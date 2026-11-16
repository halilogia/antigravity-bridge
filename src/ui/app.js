'use strict';
const translations = {
  tr: {intro:'BASLAT.cmd penceresindeki yönetim token’ını girin. API anahtarları Windows DPAPI ile saklanır.',token:'Yönetim token’ı',connect:'Bağlan',provider:'OpenCode bağlantısı',plan:'Plan',key:'API anahtarı (boş bırakırsanız kayıtlı anahtar korunur)',save:'Bağlantıyı kaydet',discover:'Model listesini getir',add:'Seçilen modelleri ekle',models:'Eklenen modeller',remove:'Kaldır',saved:'Kaydedildi.',ready:'Hazır.',keyYes:'Kayıtlı anahtar var.',keyNo:'Kayıtlı anahtar yok.',none:'Henüz model eklenmedi.',empty:'Önce model seçin.',failed:'İstek başarısız'},
  en: {intro:'Enter the admin token shown by BASLAT.cmd. API keys are stored with Windows DPAPI.',token:'Admin token',connect:'Connect',provider:'OpenCode connection',plan:'Plan',key:'API key (leave blank to preserve the saved key)',save:'Save connection',discover:'Fetch model catalog',add:'Add selected models',models:'Configured models',remove:'Remove',saved:'Saved.',ready:'Ready.',keyYes:'A key is saved.',keyNo:'No key saved.',none:'No models configured.',empty:'Select models first.',failed:'Request failed'}
};
const t = translations[document.documentElement.lang] || translations.en;
const $ = id => document.getElementById(id);
for (const [id,key] of Object.entries({intro:'intro','token-label':'token',connect:'connect','provider-title':'provider','plan-label':'plan','key-label':'key',save:'save',discover:'discover',add:'add','models-title':'models'})) $(id).textContent=t[key];
let token = '';
let catalogProvider = '';
const provider = () => `opencode-${$('plan').value}`;
async function api(path, method='GET', body) {
  const response = await fetch(`/admin${path}`,{method,headers:{'x-admin-token':token,'Content-Type':'application/json'},body:body === undefined ? undefined : JSON.stringify(body)});
  const data = await response.json();
  if (!response.ok) throw new Error(data.error?.message || `${t.failed}: ${response.status}`);
  return data;
}
function action(id, run) { $(id).addEventListener('click',async()=>{ $(id).disabled=true; try {await run();$('status').textContent=t.ready;} catch(e){$('status').textContent=e.message;} finally{$(id).disabled=false;} }); }
async function refresh() {
  const data = await api('/config');
  $('key-status').textContent=data.providers[provider()]?.hasKey ? t.keyYes : t.keyNo;
  $('models').replaceChildren();
  if (!data.models.length) $('models').textContent=t.none;
  for (const model of data.models) {
    const row=document.createElement('div');row.className='row';
    const label=document.createElement('span');label.textContent=`${model.display_name || model.id} — ${model.provider} (${model.placeholder})`;
    const button=document.createElement('button');button.textContent=t.remove;
    button.addEventListener('click',async()=>{try{await api(`/models/${encodeURIComponent(model.id)}`,'DELETE');await refresh();}catch(e){$('status').textContent=e.message;}});
    row.append(label,button);$('models').append(row);
  }
}
action('connect',async()=>{token=$('token').value.trim();await refresh();$('token').value='';});
action('save',async()=>{const key=$('key').value.trim();await api('/opencode/setup','POST',{plan:$('plan').value,...(key?{api_key:key}:{})});$('key').value='';await refresh();});
action('discover',async()=>{
  const name=provider();const data=await api(`/opencode/models?provider=${encodeURIComponent(name)}`);
  catalogProvider=name;$('catalog').replaceChildren();
  for(const model of data.models){
    const label=document.createElement('label');label.className='choice';const input=document.createElement('input');input.type='checkbox';input.value=model.id;input.disabled=model.added;input.checked=model.added;
    const text=document.createElement('span');text.textContent=model.id;label.append(input,text);$('catalog').append(label);
  }
});
action('add',async()=>{
  if(catalogProvider!==provider())throw new Error(t.empty);
  const ids=[...$('catalog').querySelectorAll('input:checked:not(:disabled)')].map(x=>x.value);
  if(!ids.length)throw new Error(t.empty);
  await api('/opencode/models','POST',{provider:provider(),ids});$('catalog').replaceChildren();await refresh();
});
$('plan').addEventListener('change',()=>{$('catalog').replaceChildren();$('key').value='';catalogProvider='';if(token)refresh().catch(e=>{$('status').textContent=e.message;});});
