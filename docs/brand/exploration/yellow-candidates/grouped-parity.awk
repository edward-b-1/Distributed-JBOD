# like stack(), but parity slabs go in their own group so they can be lit
function stack2(n,w,h,gx,gy, hs,ps, blue,acc, box, mode,   i,k,x,y,H,P,mx,W,Ht,x0,yb,a,b){
  split(hs,H," "); split(ps,P," ");
  mx=0; for(i=1;i<=n;i++) if(H[i]>mx) mx=H[i];
  W=n*w+(n-1)*gx; Ht=mx*h+(mx-1)*gy; x0=(box-W)/2; yb=(box+Ht)/2; a=""; b="";
  for(i=1;i<=n;i++){ x=x0+(i-1)*(w+gx);
    for(k=0;k<H[i];k++){ y=yb-(k+1)*h-k*gy;
      if(k==P[i]) b=b sprintf("<rect x=\"%g\" y=\"%g\" width=\"%g\" height=\"%g\" rx=\"%g\" fill=\"%s\"/>",x,y,w,h,h/2,(mode=="grad")?"url(#lit)":acc);
      else        a=a sprintf("<rect x=\"%g\" y=\"%g\" width=\"%g\" height=\"%g\" rx=\"%g\" fill=\"%s\"/>",x,y,w,h,h/2,blue);
    }
  }
  if(mode=="glow") return "<g>" a "</g><g filter=\"url(#glow)\">" b "</g>";
  return "<g>" a "</g><g>" b "</g>";
}
