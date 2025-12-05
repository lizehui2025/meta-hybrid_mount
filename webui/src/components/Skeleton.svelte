<script>
  import './Skeleton.css';
  let { width = '100%', height = '20px', style = '', borderRadius = '4px' } = $props();
</script>

<div class="skeleton" style="width: {width}; height: {height}; border-radius: {borderRadius}; {style}"></div>