# Emits the slab grid. `lit`, if set, is a filter id: the parity slabs are
# wrapped in it and a matching soft-glow filter is defined, so they read as
# lit status lights rather than flat fills. Left empty, the output is a plain
# interleaved run of rects.
function stack(n,w,h,gx,gy, hs,ps, blue,orange, box, lit,   i,k,x,y,c,H,P,mx,W,Ht,x0,yb,o,p,sd){
  split(hs,H," "); split(ps,P," ");
  mx=0; for(i=1;i<=n;i++) if(H[i]>mx) mx=H[i];
  W=n*w+(n-1)*gx; Ht=mx*h+(mx-1)*gy;
  x0=(box-W)/2; yb=(box+Ht)/2; o=""; p="";
  for(i=1;i<=n;i++){
    x=x0+(i-1)*(w+gx);
    for(k=0;k<H[i];k++){
      y=yb-(k+1)*h-k*gy;
      c=(k==P[i])?orange:blue;
      if(lit!="" && k==P[i])
        p=p sprintf("      <rect x=\"%g\" y=\"%g\" width=\"%g\" height=\"%g\" rx=\"%g\" fill=\"%s\"/>\n",x,y,w,h,h/2,c);
      else
        o=o sprintf("    <rect x=\"%g\" y=\"%g\" width=\"%g\" height=\"%g\" rx=\"%g\" fill=\"%s\"/>\n",x,y,w,h,h/2,c);
    }
  }
  if(lit=="") return o;
  # blur scales with the slab so every asset glows by the same proportion
  sd = h*0.3;
  return "    <defs><filter id=\"" lit "\" x=\"-60%\" y=\"-60%\" width=\"220%\" height=\"220%\">" \
         "<feGaussianBlur stdDeviation=\"" sd "\" result=\"b\"/>" \
         "<feMerge><feMergeNode in=\"b\"/><feMergeNode in=\"b\"/><feMergeNode in=\"SourceGraphic\"/></feMerge>" \
         "</filter></defs>\n" o "    <g filter=\"url(#" lit ")\">\n" p "    </g>\n";
}
