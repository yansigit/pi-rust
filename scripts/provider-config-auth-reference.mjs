// Capture Pi 1.3's private configured-auth composition without making provider requests.
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
const root=resolve(process.env.RPI_INTEGRATED_REFERENCE);
const {composeModelProvider}=await import(pathToFileURL(join(root,'node_modules/@earendil-works/pi-coding-agent/dist/core/provider-composer.js')));
const cases=[];
for(const configuredKey of [undefined,'configured-key','', '$RPI_FIXTURE_CONFIG_KEY','$!literal'])
for(const credential of [undefined,{type:'api_key',key:'stored-key',env:{RPI_FIXTURE_HEADER:'account-header'}},{type:'api_key',key:''}, {type:'api_key',env:{RPI_FIXTURE_HEADER:''}}, {type:'oauth',access:'access-key',refresh:'refresh-key',expires:9999999999999,env:{RPI_FIXTURE_HEADER:'oauth-header'}}])
for(const configuredHeaders of [undefined, {'X-Config':'literal-header'}, {'X-Config':'$RPI_FIXTURE_HEADER'}, {'X-Base':'configured-header',authorization:'lowercase-header'}])
for(const authHeader of [false,true])
for(const environment of [{RPI_FIXTURE_CONFIG_KEY:'environment-key',RPI_FIXTURE_HEADER:'environment-header'},{RPI_FIXTURE_CONFIG_KEY:'',RPI_FIXTURE_HEADER:''},{}]) {
 const spec={configuredKey,credential,configuredHeaders,authHeader,environment};
 const calls=[];
 // OAuth's configured headers use process env; API-key composition resolves through ctx.env.
 for(const name of ['RPI_FIXTURE_CONFIG_KEY','RPI_FIXTURE_HEADER']) {delete process.env[name];if(environment[name]!==undefined)process.env[name]=environment[name];}
 const base={id:'fixture',getModels:()=>[],auth:{apiKey:{name:'Fixture key',resolve:async input=>{
  calls.push({credential:input.credential??null});
  const key=input.credential?.key || await input.ctx.env('RPI_FIXTURE_AMBIENT');
  return key?{auth:{apiKey:key,headers:{'X-Base':'base-header'}},env:input.credential?.env,source:input.credential?.key?'stored credential':'ambient'}:undefined;
 }},oauth:{toAuth:async input=>{calls.push({oauth:input.access});return {apiKey:input.access,headers:{'X-Base':'oauth-base'}};}}}};
 const provider=composeModelProvider('fixture',base,{getProvider:()=>({apiKey:configuredKey,headers:configuredHeaders,authHeader})});
 const ctx={env:async name=>name==='RPI_FIXTURE_AMBIENT'?'ambient-key':environment[name]};
 let result;
 try {
  const value=credential?.type==='oauth'?await provider.auth.oauth.toAuth(credential):await provider.auth.apiKey.resolve({ctx,credential,signal:new AbortController().signal});
  result={value:value??null};
 } catch(error) {result={error:error.message};}
 cases.push({spec,result,calls});
}
await writeFile(process.env.RPI_REFERENCE_OUTPUT,JSON.stringify({reference:'Pi 1.3.0',cases},null,2)+'\n');
console.log(`${cases.length} configured-auth composition cases`);
