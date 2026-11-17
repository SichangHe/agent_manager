// 🧑 "Sync reading and scrolling and give a good experience like we did for adiob"
export function splitSentences(text) {
  if (typeof Intl.Segmenter === 'function') return [...new Intl.Segmenter(undefined, {granularity:'sentence'}).segment(text)].map(part=>part.segment);
  return text.match(/[^.!?]+[.!?]*(?:\s+|$)|[.!?]+/g) || [text];
}

export function createReader({container,onSeek}) {
  let active=null, following=true, currentMessages=[], currentName='', signature='';
  const follow=document.createElement('button');
  follow.type='button';follow.className='reader-follow';follow.textContent='Follow reading';follow.hidden=true;
  container.before(follow);
  function scrollActive(){
    if(!following||!active)return;
    const box=container.getBoundingClientRect(),row=active.getBoundingClientRect();
    if(row.top<box.top+12||row.bottom>box.bottom-12)container.scrollTop+=row.top-box.top-container.clientHeight/3;
  }
  function pauseFollow(){following=false;follow.hidden=!active;}
  container.addEventListener('touchstart',pauseFollow,{passive:true});
  container.addEventListener('wheel',pauseFollow,{passive:true});
  container.addEventListener('keydown',event=>{if(['ArrowUp','ArrowDown','PageUp','PageDown','Home','End'].includes(event.key))pauseFollow();});
  follow.onclick=()=>{following=true;follow.hidden=true;scrollActive();};
  function clear(){active?.classList.remove('reading');active?.removeAttribute('aria-current');active=null;follow.hidden=true;}
  function highlight(messageId,sentenceIndex){
    clear();
    active=[...container.querySelectorAll('[data-message-id]')].find(article=>article.dataset.messageId===String(messageId))?.querySelectorAll('.reader-sentence')[sentenceIndex]||null;
    if(active){active.classList.add('reading');active.setAttribute('aria-current','true');follow.hidden=following;scrollActive();}
  }
  function render(messages,name){
    const next=JSON.stringify([messages,name]);
    if(next===signature)return;
    const oldId=active?.closest('[data-message-id]')?.dataset.messageId,oldSentence=active?.dataset.sentence;
    const atBottom=container.scrollHeight-container.scrollTop-container.clientHeight<30,oldTop=container.scrollTop;
    const changedConversation=currentName!==name;
    signature=next;currentMessages=messages;currentName=name;
    container.replaceChildren(...currentMessages.map(message=>{
      const article=document.createElement('article');article.dataset.messageId=String(message.id);
      const label=document.createElement('div');label.className='reader-speaker';label.textContent=message.sender==='human'?'You':currentName;article.append(label);
      if(message.sender==='human'){const body=document.createElement('div');body.textContent=message.text;article.append(body);}
      else for(const [index,text] of splitSentences(message.text).entries()){
        const sentence=document.createElement('span');sentence.className='reader-sentence';sentence.textContent=text;sentence.dataset.sentence=String(index);sentence.tabIndex=0;sentence.setAttribute('role','button');sentence.setAttribute('aria-label','Read from: '+text.trim());
        const seek=()=>{following=true;follow.hidden=true;highlight(message.id,index);onSeek(message,index);};
        sentence.onclick=seek;sentence.onkeydown=event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();seek();}};article.append(sentence);
      }
      return article;
    }));
    clear();
    if(changedConversation){following=true;container.scrollTop=container.scrollHeight;}
    else if(oldId!==undefined){container.scrollTop=oldTop;highlight(oldId,Number(oldSentence));}
    else container.scrollTop=atBottom?container.scrollHeight:oldTop;
  }
  return {render,highlight,clear};
}

// 🧑 "Should be mostly messages on screen when not using controls"
export function installCompactControls({composer,draft,dictate}) {
  document.body.classList.add('reader-layout');
  const dock=document.createElement('div');dock.className='reader-dock';
  const toggle=document.createElement('button');toggle.type='button';toggle.textContent='Compose';toggle.setAttribute('aria-expanded','false');
  dock.append(toggle);
  for(const id of ['pause','dictate','auto','quiet']){const button=document.getElementById(id);if(button)dock.append(button);}
  composer.before(dock);
  function open(value){composer.hidden=!value;toggle.hidden=value;toggle.setAttribute('aria-expanded',String(value));toggle.textContent='Compose';}
  toggle.onclick=()=>open(composer.hidden);
  let touchStart=null;
  dock.style.touchAction='none';
  // 🧑 "the controls does not shrink down when I swipe it"
  // 🧑 "The bottom portion of the controls do not scroll." The open panel scrolls; a downward swipe collapses it only when already at its top.
  composer.style.overflowY='auto';composer.style.maxHeight='60dvh';composer.style.overscrollBehavior='contain';
  for(const surface of [dock,composer]){
    surface.addEventListener('touchstart',event=>{const touch=event.touches[0];touchStart={x:touch.clientX,y:touch.clientY,top:composer.scrollTop};},{passive:true});
    surface.addEventListener('touchend',event=>{if(!touchStart)return;const touch=event.changedTouches[0],dy=touch.clientY-touchStart.y,dx=touch.clientX-touchStart.x,top=touchStart.top;touchStart=null;if(Math.abs(dy)<=25||Math.abs(dy)<=Math.abs(dx))return;if(surface===composer&&(dy<0||top>0||composer.scrollTop>0||dy<60))return;event.preventDefault();open(dy<0);},{passive:false});
    surface.addEventListener('touchcancel',()=>{touchStart=null;},{passive:true});
  }
  const messages=document.getElementById('messages');
  for(const name of ['scroll','wheel','touchmove'])messages.addEventListener(name,()=>open(false),{passive:true});
  dictate.addEventListener('click',()=>open(true));
  draft.addEventListener('focus',()=>open(true));
  open(Boolean(draft.value.trim()));
  return {open};
}
