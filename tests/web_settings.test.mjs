import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";
import { createHttpApi } from "../frontend/http_api.js";
import { createWebSettingsView } from "../frontend/web_settings.js";
import { createWebApp } from "../frontend/web_auth.js";
import { documentFixture } from "./dom_fixture.mjs";

const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise,resolve,reject}; };
const response = (value,status=200) => new Response(status === 204 ? null : JSON.stringify(value), { status, headers: {"Content-Type":"application/json"} });
const flags = (moodleConfigured=false,webclassConfigured=false) => ({moodleConfigured,webclassConfigured});
const session = () => ({authenticated:true,userId:"internal-user",csrfToken:"a".repeat(64),expiresAt:1800001800,absoluteExpiresAt:1800043200});
const secrets = ["https://example.invalid/?token=SYNTHETIC_UI_3B","synthetic-ui-id","synthetic-ui-password","synthetic-web-password"];
function fill(ids) { ["moodle-url","webclass-id","webclass-password","reauth-password"].forEach((id,i) => { ids[id].value=secrets[i]; }); }
function empty(ids) { for (const id of ["moodle-url","webclass-id","webclass-password","reauth-password"]) assert.equal(ids[id].value,""); }
function safe(state) { for (const value of secrets) assert.ok(!JSON.stringify(state).includes(value)); }
const baseApi = () => ({ settingsStatus:async()=>flags(),reauthenticate:async()=>{},saveMoodle:async()=>{},saveWebclass:async()=>{},removeMoodle:async()=>{},removeWebclass:async()=>{} });

test("HTTP settings projects only booleans, sends CSRF/JSON and handles empty 204 responses",async()=>{
  const calls=[];
  const api=createHttpApi(async(url,options)=>{ calls.push({url,options}); return response(url==="/api/v1/session" ? session() : url.endsWith("/status") ? flags(true,false) : null,url.endsWith("/status")||url==="/api/v1/session"?200:204); });
  await api.session(); assert.deepEqual(await api.settingsStatus(),flags(true,false));
  await api.reauthenticate(secrets[3]); await api.saveMoodle(secrets[0]); await api.saveWebclass(secrets[1],secrets[2]); await api.removeMoodle(); await api.removeWebclass();
  assert.deepEqual(calls.slice(2).map(c=>[c.url,c.options.method,JSON.parse(c.options.body)]),[
    ["/api/v1/auth/reauth","POST",{password:secrets[3]}],
    ["/api/v1/settings/moodle","PUT",{calendarUrl:secrets[0]}],
    ["/api/v1/settings/webclass","PUT",{universityId:secrets[1],password:secrets[2]}],
    ["/api/v1/settings/moodle","DELETE",{}],["/api/v1/settings/webclass","DELETE",{}],
  ]);
  for(const {options} of calls.slice(2)) { assert.equal(options.headers["X-CSRF-Token"],session().csrfToken); assert.equal(options.headers["Content-Type"],"application/json"); assert.equal(options.credentials,"same-origin"); }
  for(const value of [{...flags(),calendarUrl:secrets[0]}, {},{moodleConfigured:"yes",webclassConfigured:false}]) {
    await assert.rejects(createHttpApi(async()=>response(value)).settingsStatus(),{code:"response_invalid",outcome:"unknown"});
  }
});

test("reauth denial preserves login and uses safe codes; disabled/CSRF failures still sign out",async()=>{
  for(const code of ["reauth_required","reauth_failed"]) {
    let signedOut=0;
    const api=createHttpApi(async()=>response({error:{code,message:secrets[2]}},403),()=>signedOut++);
    await assert.rejects(api.reauthenticate(secrets[3]),{code,outcome:"rejected"}); assert.equal(signedOut,0);
  }
  for(const code of ["forbidden","invalid_csrf"]) {
    let signedOut=0; const api=createHttpApi(async()=>response({error:{code}},403),()=>signedOut++);
    await assert.rejects(api.removeMoodle(),{code:"authentication_failed",outcome:"rejected"}); assert.equal(signedOut,1);
  }
});

test("settings shows configured flags and requires reauth before save with immediate input clearing",async()=>{
  const {doc,ids}=documentFixture(); const proof=deferred(); const calls=[]; let current=flags();
  const api={...baseApi(),settingsStatus:async()=>current,reauthenticate:password=>{calls.push(["reauth",password]);return proof.promise;},saveMoodle:async url=>{calls.push(["moodle",url]);current=flags(true,false);},saveWebclass:async(id,password)=>{calls.push(["webclass",id,password]);current=flags(true,true);}};
  const view=createWebSettingsView(api,doc); await view.load();
  assert.equal(ids["moodle-status"].textContent,"未設定"); assert.equal(ids["moodle-save"].disabled,true);
  fill(ids); assert.equal(await view.mutate("save_moodle"),false); empty(ids);
  fill(ids); const checking=view.reauthenticate(); empty(ids); assert.equal(ids["reauth-submit"].disabled,true); safe(view.snapshot());
  assert.equal(await view.reauthenticate(),false); assert.equal(calls.length,1);
  proof.resolve(); await checking; assert.equal(ids["moodle-save"].disabled,false);
  fill(ids); const moodle=view.mutate("save_moodle"); empty(ids); await moodle;
  assert.equal(ids["moodle-status"].textContent,"設定済み"); assert.equal(ids["moodle-save"].textContent,"置き換えて保存");
  fill(ids); await view.mutate("save_webclass"); empty(ids);
  assert.equal(ids["webclass-status"].textContent,"設定済み"); safe(view.snapshot());
  assert.deepEqual(calls,[["reauth",secrets[3]],["moodle",secrets[0]],["webclass",secrets[1],secrets[2]]]);
});

test("replace and delete preserve safe state, prevent duplicate writes and never restore inputs",async()=>{
  const {doc,ids}=documentFixture(); const pending=deferred(); let current=flags(true,true); let writes=0;
  const api={...baseApi(),settingsStatus:async()=>current,saveWebclass:()=>{writes++;return pending.promise;},removeMoodle:async()=>{current=flags(false,true);},removeWebclass:async()=>{current=flags(false,false);}};
  const view=createWebSettingsView(api,doc); await view.load(); await view.reauthenticate(); fill(ids);
  const saving=view.mutate("save_webclass"); empty(ids); assert.equal(view.snapshot().busy,true);
  assert.equal(await view.mutate("save_webclass"),false); assert.equal(await view.mutate("remove_webclass"),false); assert.equal(writes,1);
  pending.resolve(); await saving; empty(ids); safe(view.snapshot());
  await view.mutate("remove_moodle"); assert.equal(ids["moodle-status"].textContent,"未設定");
  await view.mutate("remove_webclass"); assert.equal(ids["webclass-status"].textContent,"未設定");
  assert.equal(ids["webclass-remove"].disabled,true);
});

test("acknowledged save followed by status failure stays saved, independent of read failure",async()=>{
  const {doc,ids}=documentFixture(); let reads=0;
  const view=createWebSettingsView({...baseApi(),settingsStatus:async()=>{if(++reads>1) throw {code:"network_failed"};return flags();}},doc);
  await view.load(); await view.reauthenticate(); fill(ids);
  assert.equal(await view.mutate("save_moodle"),true); assert.equal(view.snapshot().outcome,"saved");
  assert.match(view.snapshot().message,/保存しました/); assert.match(view.snapshot().error,/操作は成功しました/); empty(ids);
});

test("ambiguous replacement re-reads flags without claiming replacement success or automatically retrying",async()=>{
  for(const configured of [false,true]) {
    const {doc,ids}=documentFixture(); let reads=0,writes=0;
    const view=createWebSettingsView({...baseApi(),settingsStatus:async()=>{reads++;return flags(configured,true);},saveMoodle:async()=>{writes++;throw {code:"network_failed",outcome:"unknown",message:secrets[2]};}},doc);
    await view.load(); await view.reauthenticate(); fill(ids); assert.equal(await view.mutate("save_moodle"),false);
    assert.equal(writes,1); assert.equal(reads,2); assert.equal(view.snapshot().outcome,"unknown");
    assert.match(view.snapshot().error,/置換の成否は確認できません/); assert.match(view.snapshot().error,/自動再送/); safe(view.snapshot()); empty(ids);
    await view.load(); assert.equal(writes,1); assert.equal(view.snapshot().outcome,"unknown");
  }
});

test("expired proof and fixed error presentation do not reflect arbitrary backend text",async()=>{
  for(const error of [{code:"reauth_required",outcome:"rejected"},{code:secrets[2],outcome:"rejected",message:secrets[1]}]) {
    const {doc,ids}=documentFixture(); const view=createWebSettingsView({...baseApi(),saveMoodle:async()=>{throw error;}},doc);
    await view.load(); await view.reauthenticate(); fill(ids); await view.mutate("save_moodle"); safe(view.snapshot()); empty(ids);
    if(error.code==="reauth_required") { assert.equal(view.snapshot().reauthenticated,false); assert.equal(ids["moodle-save"].disabled,true); }
  }
});

test("disposed reads, writes and proof replies cannot restore DOM, clear a new view or start follow-up reads",async()=>{
  for(const operation of ["load","reauthenticate","mutate"]) {
    const {doc,ids}=documentFixture(); const pending=deferred(); let reads=0;
    const api={...baseApi(),settingsStatus:async()=>{reads++;return flags(true,true);}};
    const view=createWebSettingsView(api,doc); await view.load(); await view.reauthenticate();
    if(operation==="load") api.settingsStatus=()=>pending.promise;
    if(operation==="reauthenticate") api.reauthenticate=()=>pending.promise;
    if(operation==="mutate") api.saveMoodle=()=>pending.promise;
    fill(ids); const working=operation==="mutate"?view.mutate("save_moodle"):view[operation]();
    view.dispose(); empty(ids);
    const next=createWebSettingsView(baseApi(),doc); await next.load(); await next.reauthenticate(); ids["moodle-url"].value="new-view-input";
    pending.resolve(flags(true,true)); await working;
    assert.equal(ids["moodle-url"].value,"new-view-input"); assert.equal(next.snapshot().moodleConfigured,false); assert.equal(reads,1);
  }
});

test("navigation, tab/view disposal and logout clear every secret; late settings response stays suppressed",async()=>{
  const {doc,ids}=documentFixture(); const pending=deferred(); let statusReads=0;
  const app=createWebApp(doc,async(url)=>{
    if(url==="/api/v1/session") return response(session());
    if(url.startsWith("/api/v1/assignments")) return response({items:[],courses:[],totalCount:0,evaluatedAt:"2026-10-22T00:00:00Z",timeZone:"Asia/Tokyo"});
    if(url==="/api/v1/settings/status") {statusReads++;return response(flags());}
    if(url==="/api/v1/auth/logout") return response({authenticated:false});
    if(url==="/api/v1/auth/reauth") return response(null,204);
    if(url==="/api/v1/settings/moodle") return pending.promise;
    throw new Error("unexpected route");
  });
  await app.start(); await ids["show-settings"].click(); assert.equal(ids["settings-view"].hidden,false); assert.equal(ids["list-view"].hidden,true);
  fill(ids); app.clearSecrets(); empty(ids);
  fill(ids); await ids["show-list"].click(); empty(ids); assert.equal(ids["settings-view"].hidden,true);
  await ids["show-settings"].click(); fill(ids); app.suspend(); empty(ids); assert.equal(ids["settings-view"].hidden,true);
  await app.resume(); await ids["show-settings"].click();
  fill(ids); ids["reauth-form"].listeners.submit({preventDefault(){}}); await new Promise(resolve=>setImmediate(resolve));
  fill(ids); ids["moodle-form"].listeners.submit({preventDefault(){}}); empty(ids);
  await ids.logout.click(); empty(ids); assert.equal(ids["settings-view"].hidden,true); assert.equal(ids["view-navigation"].hidden,true);
  pending.resolve(response(null,204)); await new Promise(resolve=>setImmediate(resolve));
  assert.equal(statusReads,3); assert.equal(ids["settings-message"].textContent,"");
});

test("Web uses no secret storage or crypto; password input autocomplete and limits are explicit",()=>{
  for(const file of ["web_settings.js","http_api.js","web_auth.js"]) {
    const source=readFileSync(new URL(`../frontend/${file}`,import.meta.url),"utf8");
    assert.doesNotMatch(source,/localStorage|sessionStorage|indexedDB|crypto\.subtle|console\./);
  }
  const html=readFileSync(new URL("../frontend/web.html",import.meta.url),"utf8");
  for(const id of ["moodle-url","webclass-password","reauth-password"]) assert.match(html,new RegExp(`<input[^>]*id="${id}"[^>]*type="password"[^>]*autocomplete="off"[^>]*maxlength="\\d+"`));
  assert.doesNotMatch(html,/id="startup-sync"|id="sync-now"/);
});
