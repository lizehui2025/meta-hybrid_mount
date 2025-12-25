<script lang="ts">
  import { onMount } from 'svelte';
  import { store } from '../lib/store.svelte';
  import { ICONS } from '../lib/constants';
  import './NavBar.css';
  import '@material/web/icon/icon.js';
  import '@material/web/ripple/ripple.js';

  interface Props {
    activeTab: string;
    onTabChange: (id: string) => void;
  }

  let { activeTab, onTabChange }: Props = $props();
  let navContainer = $state<HTMLElement>();
  let tabRefs = $state<Record<string, HTMLButtonElement>>({});

  const ALL_TABS = [
    { id: 'status', icon: ICONS.home },
    { id: 'config', icon: ICONS.settings },
    { id: 'modules', icon: ICONS.modules },
    { id: 'logs', icon: ICONS.description },
    { id: 'granary', icon: "M12,2A10,10 0 0,0 2,12A10,10 0 0,0 12,22A10,10 0 0,0 22,12A10,10 0 0,0 12,2M12,20C7.59,20 4,16.41 4,12C4,7.59 7.59,4 12,4C16.41,4 20,7.59 20,12C20,16.41 16.41,20 12,20M12,12.5A2.5,2.5 0 0,1 9.5,10A2.5,2.5 0 0,1 12,7.5A2.5,2.5 0 0,1 14.5,10A2.5,2.5 0 0,1 12,12.5Z" },
    { id: 'winnowing', icon: "M10 18h4v-2h-4v2zM3 6v2h18V6H3zm3 7h12v-2H6v2z" },
    { id: 'info', icon: ICONS.info }
  ];

  let visibleTabs = $derived(ALL_TABS.filter(t => t.id !== 'winnowing' || store.conflicts.length > 0));

  onMount(() => {
    store.loadConflicts();
  });

  $effect(() => {
    if (activeTab && tabRefs[activeTab] && navContainer) {
      const tab = tabRefs[activeTab];
      const containerWidth = navContainer.clientWidth;
      const tabLeft = tab.offsetLeft;
      const tabWidth = tab.clientWidth;
      const scrollLeft = tabLeft - (containerWidth / 2) + (tabWidth / 2);
      
      navContainer.scrollTo({
        left: scrollLeft,
        behavior: 'smooth'
      });
    }
  });
</script>

<nav class="bottom-nav" bind:this={navContainer} style:padding-bottom={store.fixBottomNav ?
    '48px' : 'max(16px, env(safe-area-inset-bottom, 0px))'}>
  {#each visibleTabs as tab (tab.id)}
    <button 
      class="nav-tab {activeTab === tab.id ? 'active' : ''}" 
      onclick={() => onTabChange(tab.id)}
      bind:this={tabRefs[tab.id]}
      type="button"
    >
      <md-ripple></md-ripple>
      <div class="icon-container">
        <md-icon>
          <svg viewBox="0 0 24 24">
            <path d={tab.icon} style="transition: none" />
          </svg>
        </md-icon>
      </div>
      <span class="label">{store.L.tabs[tab.id as keyof typeof store.L.tabs] || tab.id}</span>
    </button>
  {/each}
</nav>