// SPDX-License-Identifier: AGPL-3.0-only
import assert from 'node:assert/strict';
import fs from 'node:fs';
const source=fs.readFileSync(new URL('../../ui/omarchy/Model.js',import.meta.url),'utf8');
const m=new Function(source+';return {parseStatus,commandFor,reading,availableModes,podMeta,levelText,levelFraction,elideError};')();
function snapshot() {
 return {schema_version:1,daemon_instance:'synthetic',revision:10,session_generation:2,connected:true,ear_policy:'off',observation_unix_ms:{'battery.case':1000},device:{address:'00:11:22:33:44:55',name:'Synthetic AirPods',model:'AirPodsPro2Usbc',capabilities:{noise_control:true,noise_off:true,adaptive:true,conversational_awareness:true,one_bud_anc:true,headset:false},listening_mode:'Adaptive',battery:{left:{level:0,status:'Discharging'},case_:{level:60,status:'Charging'}},observations:Object.fromEntries(['metadata','noise_control','control.28','control.1b','control.2e','battery.left','battery.case'].map(f=>[f,{generation:2,stale:false}])),reported_controls:{40:[1,0,0,0],27:[2,0,0,0],46:[50,0,0,0]}}};
}
let s=snapshot(),p=m.parseStatus(JSON.stringify(s));
assert.equal(p.ok,true);assert.equal(p.left.level,0);assert.equal(p.right.level,-1);assert.equal(p.caseBattery.level,60);
assert.equal(p.conversationalAwareness,true);assert.equal(p.oneBudANC,false);assert.equal(p.noiseMode,3);
assert.equal(p.adaptiveKnown,true);assert.equal(p.earDetectionBehavior,2);
assert.deepEqual(m.commandFor(p,'noise',1),['--address','00:11:22:33:44:55','--json','--noise-control','anc']);
assert.deepEqual(m.commandFor(p,'ca',false).slice(-2),['--conversation-awareness','off']);
assert.deepEqual(m.commandFor(p,'onebud',true).slice(-2),['--one-bud-anc','on']);
assert.deepEqual(m.commandFor(p,'ear',0).slice(-2),['ear','one']);
assert.deepEqual(m.commandFor(p,'adaptive',0).slice(-2),['adaptive-level','0']);
for(const [a,v] of [['adaptive',101],['adaptive',1.5],['ear',3],['noise',99],['ca','on'],['forget',true]])assert.deepEqual(m.commandFor(p,a,v),[]);
s.device.observations['battery.case'].stale=true;p=m.parseStatus(JSON.stringify(s));
assert.equal(p.caseBattery.level,60);assert.equal(p.caseBattery.charging,false);assert.equal(m.podMeta(p.caseBattery,121000),'Last reported · 2m ago');assert.equal(p.caseBattery.reportedAt,1000);
s.device.observations.metadata.generation=1;p=m.parseStatus(JSON.stringify(s));assert.deepEqual(m.availableModes(p.supportsNoiseControl,p.supportsNoiseOff,p.supportsAdaptive),[]);assert.deepEqual(m.commandFor(p,'noise',1),[]);
s=snapshot();s.device.reported_controls[40]=[3,0,0,0];p=m.parseStatus(JSON.stringify(s));assert.equal(p.supportsConversationalAwareness,false);assert.deepEqual(m.commandFor(p,'ca',true),[]);
s=snapshot();s.device.observations['control.2e'].stale=true;p=m.parseStatus(JSON.stringify(s));assert.deepEqual(m.commandFor(p,'adaptive',20),[]);
s=snapshot();s.device.listening_mode='Transparency';p=m.parseStatus(JSON.stringify(s));assert.deepEqual(m.commandFor(p,'adaptive',20),[]);
s=snapshot();delete s.device.capabilities;p=m.parseStatus(JSON.stringify(s));assert.equal(p.supportsNoiseControl,false);
s=snapshot();s.connected=false;p=m.parseStatus(JSON.stringify(s));assert.deepEqual(m.commandFor(p,'ear',1),[]);
s=snapshot();s.device.battery.case_={level:0,status:'Disconnected'};p=m.parseStatus(JSON.stringify(s));assert.equal(p.caseBattery.level,-1);
s=snapshot();s.device.battery.case_.level=101;p=m.parseStatus(JSON.stringify(s));assert.equal(p.caseBattery.level,-1);
s=snapshot();s.device.address='00:11; echo wrong';p=m.parseStatus(JSON.stringify(s));assert.deepEqual(m.commandFor(p,'noise',1),[]);
for(const raw of ['','null','{}','[]','invalid',JSON.stringify({...snapshot(),schema_version:2}), ' '.repeat(131073)])assert.equal(m.parseStatus(raw).ok,false);
assert.equal(m.parseStatus(JSON.stringify({...snapshot(),schema_version:2})).schemaTooNew,true);
s=snapshot();s.device.model='AirPodsMax2';s.device.capabilities.headset=true;s.device.battery.headset={level:80,status:'Discharging'};s.device.observations['battery.headset']={generation:2,stale:false};p=m.parseStatus(JSON.stringify(s));assert.equal(p.isHeadset,true);assert.equal(p.headset.level,80);
assert.equal(m.levelText(-1),'--');assert.equal(m.levelText(0),'0%');assert.equal(m.levelFraction(100),1);assert.equal(m.levelFraction(-1),0);assert.equal(m.elideError('a'.repeat(300)).length,138);
console.log('Native Omarchy model: lifecycle, battery provenance, capabilities and typed action guards passed');
